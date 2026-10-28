//! Tokio-based session server.
//!
//! Architecture (one runtime per `mn --server` process, current_thread flavor):
//!
//!  - **Main task** (`mainloop`) owns:
//!      * the ring buffer
//!      * the PTY master (`AsyncFd<OwnedFd>`)
//!      * the listening Unix socket
//!      * the child process handle (`tokio::process::Child`)
//!      * a `Vec<ClientHandle>` describing each registered client
//!      * the receive end of an unbounded control mpsc that intake / reader /
//!        writer tasks send events on.
//!
//!  - **Intake task** (one per accepted connection): reads the `Hello`
//!    packet, validates protocol version + peer UID, then forwards
//!    `ControlMsg::Hello{stream, hello}` to the main task. The main task
//!    supplies authoritative `Welcome` metadata without registering queries.
//!
//!  - **Per-client writer task**: owns the write half of the socket and a
//!    bounded `mpsc::Receiver<OutMsg>`. On startup it sends the ring
//!    snapshot as `Replay` chunks + `ReplayEnd`, signals
//!    `ControlMsg::ClientLive{id}`, then drains the channel forever.
//!
//!  - **Per-client reader task**: owns the read half. Parses packets and
//!    forwards them as `ControlMsg::Input/Resize/Detach{id, ...}`. On
//!    EOF or error, sends `ControlMsg::ClientGone{id}`.
//!
//! Backpressure: PTY fan-out queues output for every registered client,
//! including clients still replaying their snapshot. The bounded per-client
//! outbound channel pauses PTY ingestion when any client has insufficient
//! capacity. Control events and new connections remain responsive. Replay must
//! finish before the writer drains queued output; no post-snapshot bytes are skipped.

use crate::protocol::{
    self, ClientFlags, Hello, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
    recv_packet_async as recv_packet, send_packet_async as send_packet,
};
use crate::ring::RingBuffer;

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncWriteExt, Interest};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, mpsc};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Capacity of each client's outbound channel, in packets. Each `Content`
/// packet is at most `HEADER_SIZE + MAX_PAYLOAD` ≈ 4 KiB. 64 packets ≈
/// 256 KiB — matches the original byte-budget per client.
const OUTBOUND_CAP: usize = 64;
const READ_BUF_SIZE: usize = 4096;
const READ_PACKETS: usize = READ_BUF_SIZE.div_ceil(MAX_PAYLOAD);
const CLIENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
// Pause the PTY EOF deadline under backpressure; client deadlines bound that pause.
const PTY_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
// Leave headroom for the CLI's two-second lock-release deadline.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
const REAP_TIMEOUT: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

/// Open a PTY pair using libc::openpty (available on macOS and Linux).
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut leader: libc::c_int = -1;
    let mut follower: libc::c_int = -1;
    let ret = unsafe {
        libc::openpty(
            &mut leader,
            &mut follower,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    let (leader, follower) =
        unsafe { (OwnedFd::from_raw_fd(leader), OwnedFd::from_raw_fd(follower)) };
    // Setup is single-threaded, before the runtime or child is started.
    // dup2 onto the command's stdio clears CLOEXEC on 0/1/2, but the
    // original master and slave must not survive the command's exec.
    rustix::io::fcntl_setfd(&leader, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&follower, rustix::io::FdFlags::CLOEXEC)?;
    Ok((leader, follower))
}

#[cfg(target_os = "macos")]
fn verify_peer_uid_std(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    let my_uid = rustix::process::getuid().as_raw();
    let mut peer_uid: libc::uid_t = 0;
    let mut peer_gid: libc::gid_t = 0;
    let ret = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    if peer_uid != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer uid {peer_uid} does not match server uid {my_uid}"),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer_uid_std(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    use std::os::fd::AsFd;
    let my_uid = rustix::process::getuid().as_raw();
    let cred = rustix::net::sockopt::socket_peercred(stream.as_fd())?;
    let peer_uid = cred.uid.as_raw();
    if peer_uid != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer uid {peer_uid} does not match server uid {my_uid}"),
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn verify_peer_uid_std(_stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Server argument parsing
// ---------------------------------------------------------------------------

struct ServerOpts {
    ready_fd: i32,
    rows: u16,
    cols: u16,
    ring_size: usize,
    socket_path: PathBuf,
    command: Vec<String>,
}

fn arg_err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn parse_server_args(args: &[String]) -> io::Result<ServerOpts> {
    let mut ready_fd = None;
    let mut rows = 24u16;
    let mut cols = 80u16;
    let mut ring_size = 1024 * 1024usize;
    let mut socket_path = None;
    let mut command = Vec::new();

    if args.is_empty() {
        return Err(arg_err("--server requires a session name"));
    }
    let _session_name = args[0].clone();
    let mut i = 1;
    let mut past_separator = false;

    while i < args.len() {
        if past_separator {
            command.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => past_separator = true,
            "--ready-fd" => {
                i += 1;
                ready_fd = Some(
                    args.get(i)
                        .ok_or_else(|| arg_err("--ready-fd requires a value"))?
                        .parse::<i32>()
                        .map_err(|e| arg_err(format!("invalid --ready-fd: {e}")))?,
                );
            }
            "--rows" => {
                i += 1;
                rows = args
                    .get(i)
                    .ok_or_else(|| arg_err("--rows requires a value"))?
                    .parse()
                    .map_err(|e| arg_err(format!("invalid --rows: {e}")))?;
            }
            "--cols" => {
                i += 1;
                cols = args
                    .get(i)
                    .ok_or_else(|| arg_err("--cols requires a value"))?
                    .parse()
                    .map_err(|e| arg_err(format!("invalid --cols: {e}")))?;
            }
            "--ring-size" => {
                i += 1;
                ring_size = args
                    .get(i)
                    .ok_or_else(|| arg_err("--ring-size requires a value"))?
                    .parse()
                    .map_err(|e| arg_err(format!("invalid --ring-size: {e}")))?;
            }
            "--socket-path" => {
                i += 1;
                socket_path = Some(PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| arg_err("--socket-path requires a value"))?,
                ));
            }
            other => return Err(arg_err(format!("unknown server option: {other}"))),
        }
        i += 1;
    }

    Ok(ServerOpts {
        ready_fd: ready_fd.ok_or_else(|| arg_err("--ready-fd is required"))?,
        rows,
        cols,
        ring_size,
        socket_path: socket_path.ok_or_else(|| arg_err("--socket-path is required"))?,
        command,
    })
}

// ---------------------------------------------------------------------------
// Server entry point — sync setup, then spin up tokio runtime
// ---------------------------------------------------------------------------

pub fn run_server(args: &[String]) -> Result<(), crate::Error> {
    let opts = parse_server_args(args)?;
    let ready_fd = unsafe { OwnedFd::from_raw_fd(opts.ready_fd) };
    let mut reporter = crate::startup::Reporter::new(ready_fd)?;
    let result = run_server_inner(opts, &mut reporter);
    if let Err(ref error) = result {
        reporter.failure(&error.to_string());
    }
    result
}

fn run_server_inner(
    opts: ServerOpts,
    reporter: &mut crate::startup::Reporter,
) -> Result<(), crate::Error> {
    crate::startup::validate_ring_size(opts.ring_size)?;
    eprintln!(
        "[mn-srv] startup pid={} session={:?} command={:?}",
        std::process::id(),
        opts.socket_path.file_name().unwrap_or_default(),
        opts.command
    );

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[mn-srv] PANIC: {info}");
        default_hook(info);
    }));

    // Detach from controlling terminal. setsid may fail if we're already a
    // process group leader — that's fine.
    let _ = rustix::process::setsid();
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // Acquire the session lock BEFORE binding — kernel releases on death.
    let lock_path = crate::socket::lock_path_for_socket(&opts.socket_path)?;
    // Discovery probes also use flock. Allow brief probes to finish.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let _lock_fd = loop {
        if let Some(fd) = crate::socket::try_acquire_lock(&lock_path)? {
            break fd;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "another server already holds the session lock: {}",
                lock_path.display()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // Bind + listen BEFORE spawning the child — fail early.
    let std_listener = bind_listen(&opts.socket_path)
        .map_err(|e| format!("bind {}: {e}", opts.socket_path.display()))?;
    // Reverse declaration order drops socket cleanup before the lock.
    let _socket_cleanup = crate::socket::SocketCleanup::new(&opts.socket_path)?;
    std_listener.set_nonblocking(true)?;

    // Open PTY and set initial size.
    let (leader_fd, follower_fd) = open_pty().map_err(|e| format!("openpty: {e}"))?;
    let winsize = rustix::termios::Winsize {
        ws_row: opts.rows,
        ws_col: opts.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = rustix::termios::tcsetwinsize(&leader_fd, winsize);

    // Build the runtime. Current-thread: low overhead, no Send bounds.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        // Install handlers before spawning. The outer select remains responsive
        // even when mainloop is awaiting a full client queue.
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut child_exit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;

        // Spawn the child *inside* the runtime so tokio can wire SIGCHLD.
        let mut child = spawn_child(&opts.command, follower_fd)
            .map_err(|e| format!("spawn {:?}: {e}", opts.command))?;

        let result = async {
            // PTY leader must be non-blocking for AsyncFd.
            set_nonblocking(&leader_fd)?;
            let leader = std::sync::Arc::new(
                AsyncFd::with_interest(leader_fd, Interest::READABLE | Interest::WRITABLE)
                    .map_err(|e| format!("AsyncFd PTY: {e}"))?,
            );

            let listener = UnixListener::from_std(std_listener)?;

            tokio::select! {
                biased;
                _ = terminate.recv() => {
                    let foreground = foreground_group(leader.get_ref());
                    drop(leader);
                    // Cancellation aborts mainloop's tasks. Let their PTY clones
                    // close so a child cannot block exiting on terminal flush.
                    tokio::task::yield_now().await;
                    shutdown_child(&mut child, foreground).await
                },
                result = mainloop(listener, leader.clone(), &mut child, &mut child_exit, opts.ring_size, reporter) => result,
            }
        }.await;

        // Also covers failures in post-spawn PTY/listener registration. Child::id
        // becomes None after wait(), so never signal an old, reusable group ID.
        let cleanup = shutdown_child(&mut child, None).await;
        result.and(cleanup)
    })?;

    eprintln!("[mn-srv] exit clean");
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket binding
// ---------------------------------------------------------------------------

fn bind_listen(path: &std::path::Path) -> io::Result<std::os::unix::net::UnixListener> {
    // Caller holds the session lock.
    crate::socket::remove_socket(path)?;
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

// ---------------------------------------------------------------------------
// Child spawning (tokio::process::Command)
// ---------------------------------------------------------------------------

fn spawn_child(command: &[String], follower_fd: OwnedFd) -> io::Result<tokio::process::Child> {
    use std::process::Stdio;

    let (cmd, args) = if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        (shell, Vec::new())
    } else {
        (command[0].clone(), command[1..].to_vec())
    };

    let follower_for_stdout = follower_fd.try_clone()?;
    let follower_for_stderr = follower_fd.try_clone()?;

    let mut command = tokio::process::Command::new(&cmd);
    command
        .args(&args)
        .stdin(Stdio::from(follower_fd))
        .stdout(Stdio::from(follower_for_stdout))
        .stderr(Stdio::from(follower_for_stderr))
        // Don't let tokio kill the child when the Child handle is dropped —
        // we manage lifetime explicitly.
        .kill_on_drop(false);

    unsafe {
        command.pre_exec(|| {
            // The server ignores HUP to survive detachment; the command must
            // not inherit that disposition. Only async-signal-safe libc calls
            // are allowed between fork and exec.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) < 0
                || libc::setsid() < 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn()
}

// Observe exit without reaping: group shutdown must precede PID reuse.
async fn observe_child_exit(pid: u32, signal: &mut tokio::signal::unix::Signal) -> io::Result<u32> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } != 0 {
            return Ok(if info.si_code == libc::CLD_EXITED {
                unsafe { info.si_status() as u32 }
            } else {
                1
            });
        }
        if signal.recv().await.is_none() {
            return Err(io::Error::other("child signal stream closed"));
        }
    }
}

fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // Darwin reports EPERM rather than ESRCH when the owned group contains
    // only zombies. Its live, same-user members would be signalable.
    let zombie_group = cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EPERM);
    if error.raw_os_error() == Some(libc::ESRCH) || zombie_group {
        Ok(())
    } else {
        Err(io::Error::new(
            error.kind(),
            format!("signal {signal} to group {pgid}: {error}"),
        ))
    }
}

fn foreground_group(leader: &OwnedFd) -> Option<libc::pid_t> {
    let group = unsafe { libc::tcgetpgrp(leader.as_raw_fd()) };
    (group > 0).then_some(group)
}

/// End the managed group and the terminal's active foreground group. HUP lets
/// shells forward hangup to background jobs; detached/new-session descendants
/// are not swept. Normal leader exit can clear the terminal's foreground group
/// before observation. Leaderless foreground groups cannot be revalidated.
/// The foreground identity check minimizes, but cannot atomically eliminate,
/// group-ID reuse races on portable Unix. Never use a reaped child's old PID.
async fn shutdown_child(
    child: &mut tokio::process::Child,
    foreground: Option<libc::pid_t>,
) -> Result<(), crate::Error> {
    let Some(pid) = child.id().map(|pid| pid as libc::pid_t) else {
        return Ok(());
    };
    let foreground = foreground.filter(|&group| group != pid);
    let mut error = None;
    for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGCONT] {
        if let Err(e) = signal_group(pid, signal) {
            error = Some(e);
        }
        if let Some(group) = foreground {
            // Foreground group leaders can exit during the grace period. A
            // reused PID must not direct a signal into an unrelated session.
            if unsafe { libc::getsid(group) } == pid
                && let Err(e) = signal_group(group, signal)
            {
                error = Some(e);
            }
        }
    }
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    if let Some(group) = foreground
        && unsafe { libc::getsid(group) } == pid
        && let Err(e) = signal_group(group, libc::SIGKILL)
    {
        error = Some(e);
    }
    if let Err(e) = signal_group(pid, libc::SIGKILL) {
        error = Some(e);
    }
    // Always try to reap, including when a signal failed. A pathological child
    // stuck in the kernel cannot hold the session lock indefinitely.
    tokio::time::timeout(REAP_TIMEOUT, async {
        loop {
            if child.try_wait()?.is_some() {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "managed child did not exit"))??;
    match error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Server-side state and messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Replaying, // writer hasn't finished sending the snapshot yet
    Live,      // replay finished; eligible to become resize controller
}

#[derive(Debug)]
enum OutMsg {
    Packet(Packet),
    Shutdown, // writer should drain pending then exit
}

struct ClientHandle {
    id: u64,
    flags: ClientFlags,
    state: ClientState,
    is_controller: bool,
    out_tx: mpsc::Sender<OutMsg>,
    // Coalesced control state stays bounded even if the outbound queue is full.
    resize_pending: bool,
    terminal_pending: Option<Packet>,
    shutdown_pending: bool,
    drain_deadline: Option<tokio::time::Instant>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl ClientHandle {
    fn controls_pending(&self) -> bool {
        self.resize_pending || self.terminal_pending.is_some() || self.shutdown_pending
    }

    fn finish(&mut self, packet: Packet, now: tokio::time::Instant) {
        // This slot is independent of the bounded content channel. Flush it
        // behind all accepted output, then close; never await a slow socket.
        self.resize_pending = false;
        self.terminal_pending = Some(packet);
        self.shutdown_pending = true;
        self.drain_deadline = Some(now + CLIENT_DRAIN_TIMEOUT);
    }

    fn flush_controls(&mut self) {
        if self.resize_pending {
            if self
                .out_tx
                .try_send(OutMsg::Packet(Packet::empty(MsgType::ResizeReq)))
                .is_err()
            {
                return;
            }
            self.resize_pending = false;
        }
        if let Some(packet) = self.terminal_pending.as_ref() {
            if self
                .out_tx
                .try_send(OutMsg::Packet(packet.clone()))
                .is_err()
            {
                return;
            }
            self.terminal_pending = None;
        }
        if self.shutdown_pending && self.out_tx.try_send(OutMsg::Shutdown).is_ok() {
            self.shutdown_pending = false;
        }
    }
}

enum ControlMsg {
    /// Intake validated Hello; mainloop decides whether to query or attach.
    Hello {
        stream: tokio::net::UnixStream,
        hello: Hello,
    },
    /// Writer task finished sending the replay; client transitions to Live.
    ClientLive { id: u64 },
    /// Writer flushed the terminal result and shut down its write half.
    ClientFinished,
    /// Reader task: client requested a resize.
    Resize { id: u64, rows: u16, cols: u16 },
    /// Reader task: client requested disconnect.
    Detach { id: u64 },
    /// Reader OR writer task: socket closed / I/O error.
    ClientGone { id: u64 },
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn mainloop(
    listener: UnixListener,
    leader: std::sync::Arc<AsyncFd<OwnedFd>>,
    child: &mut tokio::process::Child,
    child_exit: &mut tokio::signal::unix::Signal,
    ring_size: usize,
    reporter: &mut crate::startup::Reporter,
) -> Result<(), crate::Error> {
    let child_pid = child.id().unwrap_or(0);
    let mut child = Some(child);
    let mut cleanup = None;
    // Cancellation must close client-owned PTY clones as well as sockets.
    let mut tasks = tokio::task::JoinSet::new();
    let mut ring = RingBuffer::new(ring_size);
    let mut clients: Vec<ClientHandle> = Vec::new();
    let mut next_id: u64 = 1;
    let mut child_running = true;
    let mut exit_status: Option<u32> = None;
    let mut exit_notified = false;
    let mut terminal: Option<Packet> = None;
    let mut drain_failure: Option<String> = None;
    let mut pty_deadline = None;
    let mut pty_budget = PTY_DRAIN_TIMEOUT;
    let mut pty_clock = None;
    // On Linux, reading the PTY master after the slave has closed returns
    // EIO (vs. macOS returning 0). Either way, once we've seen EOF we must
    // stop polling the readable arm — otherwise AsyncFd keeps reporting
    // ready and we busy-loop, starving child.wait() and listener.accept().
    let mut pty_eof = false;

    let mut pty_log = std::env::var("MNEME_PTY_LOG").ok().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let capacity_changed = std::sync::Arc::new(Notify::new());
    let pty_write_lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut read_buf = vec![0u8; READ_BUF_SIZE];

    // Report success only after runtime/PTY registration and all initial
    // main-loop state (including the ring allocation) have been set up.
    reporter.success()?;

    loop {
        // A queued Exit is not notification. Preserve output for retries
        // unless a writer confirmed the result, and finish group cleanup.
        if terminal.is_some() && clients.is_empty() && exit_notified && cleanup.is_none() {
            eprintln!("[mn-srv] mainloop exit: child_gone+no_clients+notified");
            break;
        }

        let now = tokio::time::Instant::now();
        let before = clients.len();
        clients.retain(|c| {
            let expired = c.drain_deadline.is_some_and(|deadline| deadline <= now);
            if expired {
                eprintln!("[mn-srv] client {} disconnected with undelivered output: exit drain deadline exceeded", c.id);
            }
            !expired
        });
        if clients.len() != before && child_running {
            elect_controller(&mut clients);
        }
        if !child_running && terminal.is_none() {
            if !pty_eof {
                pty_deadline =
                    drain_deadline(&mut pty_budget, &mut pty_clock, now, output_ready(&clients));
                if pty_deadline.is_some_and(|deadline| now >= deadline) {
                    let error = "final output incomplete: timed out waiting for PTY EOF (inherited slave descriptor)";
                    eprintln!("[mn-srv] {error}");
                    drain_failure = Some(error.into());
                    pty_eof = true;
                }
            }
            if pty_eof {
                terminal = Some(match &drain_failure {
                    Some(error) => Packet::error(error),
                    None => Packet::exit(exit_status.expect("observed child status")),
                });
                pty_deadline = None;
                for c in &mut clients {
                    c.finish(terminal.as_ref().unwrap().clone(), now);
                }
            }
        }
        for c in &mut clients {
            c.flush_controls();
        }
        let drain_deadline = clients
            .iter()
            .filter_map(|c| c.drain_deadline)
            .chain(pty_deadline)
            .min();
        // Mainloop is the only sender. With no await between this capacity
        // check and fan-out, a read cannot overrun any client's bounded queue.
        let output_ready = output_ready(&clients);
        tokio::select! {
            // Randomized selection prevents a continuously readable PTY or
            // busy control channel from starving accepts and other controls.
            _ = capacity_changed.notified() => {}
            _ = async {
                match drain_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}

            // Do not retain completed task handles in long-lived sessions.
            _ = tasks.join_next(), if !tasks.is_empty() => {}

            result = async { cleanup.as_mut().unwrap().await }, if cleanup.is_some() => {
                cleanup = None;
                result?;
            }

            // Child exit ------------------------------------------------
            res = observe_child_exit(child_pid, child_exit), if child_running => {
                let status = res?;
                // Retain WNOWAIT's reserved identity until group cleanup
                // finishes, but poll its grace/reap waits alongside PTY reads
                // and controls. Cancellation leaves outer shutdown responsible.
                let child = child.take().expect("cleanup starts once");
                let foreground = foreground_group(leader.get_ref());
                cleanup = Some(Box::pin(async move {
                    if let Err(error) = shutdown_child(child, foreground).await {
                        if child.id().is_some() {
                            return Err(error);
                        }
                        eprintln!("[mn-srv] group cleanup: {error}");
                    }
                    Ok::<_, crate::Error>(())
                }));
                child_running = false;
                exit_status = Some(status);
                eprintln!("[mn-srv] child exited status={status} live_clients={}", clients.len());
                let deadline = tokio::time::Instant::now() + CLIENT_DRAIN_TIMEOUT;
                for c in &mut clients {
                    c.drain_deadline = Some(deadline);
                }
                // No Exit yet: PTY EOF must follow the final accepted bytes.
                // With no clients we retain the finalized snapshot and result.
            }

            // PTY readable ----------------------------------------------
            // Note: we keep polling even after `child_running` flips false,
            // because data the child wrote just before exiting may still be
            // sitting in the PTY master read buffer. We only stop on EIO/EOF.
            guard = leader.readable(), if !pty_eof && output_ready => {
                let mut guard = match guard {
                    Ok(g) => g,
                    Err(error) => return Err(io::Error::other(format!("PTY readiness failed; output may be incomplete: {error}")).into()),
                };
                match guard.try_io(|inner| {
                    let fd = inner.get_ref().as_fd();
                    match rustix::io::read(fd, &mut read_buf) {
                        Ok(0) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                        Ok(n) => Ok(n),
                        Err(rustix::io::Errno::AGAIN) => Err(io::ErrorKind::WouldBlock.into()),
                        Err(e) => Err(io::Error::from(e)),
                    }
                }) {
                    Ok(Ok(n)) => {
                        let data = &read_buf[..n];
                        if let Some(ref mut f) = pty_log {
                            use std::io::Write;
                            let _ = f.write_all(data);
                        }
                        ring.write(data);

                        try_fan_out(&clients, data);
                    }
                    Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Ok(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        // PTY closed (macOS reports read==0). Stop polling
                        // and let child.wait() finish.
                        pty_eof = true;
                    }
                    Ok(Err(e)) => {
                        // Linux returns EIO on PTY master after the slave
                        // closes. Treat as EOF so we don't busy-loop.
                        if e.raw_os_error() == Some(libc::EIO) {
                            pty_eof = true;
                        } else {
                            if child_running {
                                return Err(io::Error::other(format!("PTY read failed; output may be incomplete: {e}")).into());
                            }
                            drain_failure = Some(format!("final output incomplete: PTY read failed: {e}"));
                            eprintln!("[mn-srv] {}", drain_failure.as_ref().unwrap());
                            pty_eof = true;
                        }
                    }
                    Err(_would_block) => { /* try_io returns Err if ready was spurious; loop */ }
                }
            }

            // Listener accept -------------------------------------------
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        // Peer-uid check on the std stream (rustix/getpeereid).
                        let std_stream = match stream.into_std() {
                            Ok(s) => s,
                            Err(e) => { eprintln!("[mn-srv] into_std: {e}"); continue; }
                        };
                        if let Err(e) = verify_peer_uid_std(&std_stream) {
                            eprintln!("[mn-srv] reject peer: {e}");
                            continue;
                        }
                        // Already non-blocking from the listener; convert back.
                        std_stream.set_nonblocking(true).ok();
                        let stream = match tokio::net::UnixStream::from_std(std_stream) {
                            Ok(s) => s,
                            Err(e) => { eprintln!("[mn-srv] from_std: {e}"); continue; }
                        };
                        // Spawn intake. It owns the stream until handshake done.
                        let ctx = ctrl_tx.clone();
                        tasks.spawn(intake_task(stream, ctx));
                    }
                    Err(e) => eprintln!("[mn-srv] accept: {e}"),
                }
            }

            // Control events from intake / reader / writer tasks --------
            Some(msg) = ctrl_rx.recv() => {
                match msg {
                    ControlMsg::Hello { mut stream, hello } => {
                        let attaching = hello.intent == Intent::Attach;
                        let welcome = Packet::welcome(&protocol::Welcome {
                            version: PROTOCOL_VERSION,
                            server_pid: std::process::id(),
                            child_pid,
                            child_running,
                            exit_status: exit_status.unwrap_or(0) as u8,
                            // Attach includes its new registration, even during replay.
                            client_count: u16::try_from(clients.len() + usize::from(attaching)).unwrap_or(u16::MAX),
                            ring_size: ring.capacity() as u32,
                            ring_used: ring.len() as u32,
                        });
                        if !attaching {
                            // Queries neither register nor elect a controller. Socket I/O
                            // stays outside mainloop so an unresponsive peer cannot stall it.
                            tasks.spawn(async move {
                                let _ = tokio::time::timeout(HELLO_TIMEOUT, send_packet(&mut stream, &welcome)).await;
                            });
                            continue;
                        }
                        let id = next_id; next_id += 1;
                        // Snapshot and registration have no intervening await:
                        // subsequent PTY bytes go into this client's queue.
                        let snapshot = ring.snapshot();
                        let (out_tx, out_rx) = mpsc::channel::<OutMsg>(OUTBOUND_CAP);
                        let (read_half, mut write_half) = stream.into_split();
                        clients.push(ClientHandle {
                            id,
                            flags: hello.flags,
                            state: ClientState::Replaying,
                            is_controller: false,
                            out_tx,
                            resize_pending: false,
                            terminal_pending: terminal.clone(),
                            shutdown_pending: terminal.is_some(),
                            drain_deadline: (!child_running).then(|| tokio::time::Instant::now() + CLIENT_DRAIN_TIMEOUT),
                            tasks: vec![
                                tasks.spawn({
                                    let ctrl_tx = ctrl_tx.clone();
                                    let capacity_changed = capacity_changed.clone();
                                    async move {
                                        if !matches!(
                                            tokio::time::timeout(HELLO_TIMEOUT, send_packet(&mut write_half, &welcome)).await,
                                            Ok(Ok(()))
                                        ) {
                                            let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
                                            return;
                                        }
                                        writer_task(id, write_half, snapshot, out_rx, ctrl_tx, capacity_changed).await;
                                    }
                                }),
                                tasks.spawn(reader_task(id, read_half, ctrl_tx.clone(), leader.clone(), pty_write_lock.clone(), hello.flags)),
                            ],
                        });
                    }
                    ControlMsg::ClientLive { id } => {
                        if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                            c.state = ClientState::Live;
                            if child_running {
                                elect_controller(&mut clients);
                            }
                        }
                    }
                    ControlMsg::ClientFinished => exit_notified = true,
                    ControlMsg::Resize { id, rows, cols } => {
                        let is_ctrl = clients.iter().find(|c| c.id == id).map(|c| c.is_controller).unwrap_or(false);
                        if is_ctrl && child_running {
                            let ws = rustix::termios::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                            let _ = rustix::termios::tcsetwinsize(leader.get_ref(), ws);
                            if let Some(pgid) = rustix::process::Pid::from_raw(child_pid as i32) {
                                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::WINCH);
                            }
                        }
                    }
                    // Detach is an explicit disconnect, not an output drain.
                    // Removing it releases backpressure even if it never reads.
                    ControlMsg::Detach { id } | ControlMsg::ClientGone { id } => {
                        if let Some(pos) = clients.iter().position(|c| c.id == id) {
                            clients.remove(pos);
                            if child_running {
                                elect_controller(&mut clients);
                            }
                        }
                    }
                }
            }
        }
    }

    match drain_failure {
        Some(error) => Err(io::Error::other(error).into()),
        None => Ok(()),
    }
}

// Charge only unblocked time, but never replenish the budget when a client
// briefly fills its queue. Client deadlines independently bound blocked time.
fn drain_deadline(
    budget: &mut Duration,
    active_since: &mut Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    ready: bool,
) -> Option<tokio::time::Instant> {
    if let Some(since) = active_since.take() {
        *budget = budget.saturating_sub(now - since);
    }
    if budget.is_zero() {
        Some(now)
    } else if ready {
        *active_since = Some(now);
        Some(now + *budget)
    } else {
        None
    }
}

fn output_ready(clients: &[ClientHandle]) -> bool {
    clients.iter().all(|c| {
        c.out_tx.is_closed() || (!c.controls_pending() && c.out_tx.capacity() >= READ_PACKETS)
    })
}

fn try_fan_out(clients: &[ClientHandle], data: &[u8]) {
    for chunk in data.chunks(MAX_PAYLOAD) {
        let pkt = Packet::content(chunk);
        for c in clients {
            // Includes replaying clients. ReplayEnd precedes queue draining.
            match c.out_tx.try_send(OutMsg::Packet(pkt.clone())) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    unreachable!("PTY read exceeded reserved outbound capacity")
                }
            }
        }
    }
}

// Exercise the same admission check and fan-out as mainloop in replay tests.
#[cfg(test)]
async fn fan_out(clients: &[ClientHandle], data: &[u8], capacity_changed: &Notify) {
    for chunk in data.chunks(READ_BUF_SIZE) {
        while !output_ready(clients) {
            capacity_changed.notified().await;
        }
        try_fan_out(clients, chunk);
    }
}

async fn write_pty(leader: &AsyncFd<OwnedFd>, mut buf: &[u8]) {
    while !buf.is_empty() {
        let mut guard = match leader.writable().await {
            Ok(g) => g,
            Err(_) => return,
        };
        match guard.try_io(|inner| {
            let fd = inner.get_ref().as_fd();
            match rustix::io::write(fd, buf) {
                Ok(0) => Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => Ok(n),
                Err(rustix::io::Errno::AGAIN) => Err(io::ErrorKind::WouldBlock.into()),
                Err(e) => Err(io::Error::from(e)),
            }
        }) {
            Ok(Ok(n)) => buf = &buf[n..],
            Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Ok(Err(_)) => return,
            Err(_would_block) => continue,
        }
    }
}

fn elect_controller(clients: &mut [ClientHandle]) {
    let old = clients.iter().position(|c| c.is_controller);
    let mut new_idx = None;
    for (i, c) in clients.iter().enumerate().rev() {
        if c.state == ClientState::Live
            && !c.flags.contains(ClientFlags::READONLY)
            && !c.flags.contains(ClientFlags::LOW_PRIORITY)
        {
            new_idx = Some(i);
            break;
        }
    }
    if new_idx == old {
        return;
    }
    for c in clients.iter_mut() {
        c.is_controller = false;
    }
    if let Some(i) = new_idx {
        clients[i].is_controller = true;
        clients[i].resize_pending = true;
    }
}

// ---------------------------------------------------------------------------
// Intake task — validates Hello before requesting authoritative metadata
// ---------------------------------------------------------------------------

async fn intake_task(
    mut stream: tokio::net::UnixStream,
    ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
) {
    // Bounded handshake time so a malicious or stuck peer can't hold a
    // connection slot forever.
    let res = tokio::time::timeout(HELLO_TIMEOUT, async {
        let pkt = recv_packet(&mut stream).await?;
        if pkt.msg_type != MsgType::Hello {
            let _ = send_packet(&mut stream, &Packet::error("expected Hello")).await;
            return Err(io::Error::other("not Hello"));
        }
        let hello = pkt
            .parse_hello()
            .ok_or_else(|| io::Error::other("malformed Hello"))?;
        if hello.version != PROTOCOL_VERSION {
            let _ = send_packet(
                &mut stream,
                &Packet::error(&format!(
                    "protocol version mismatch: client={}, server={}",
                    hello.version, PROTOCOL_VERSION
                )),
            )
            .await;
            return Err(io::Error::other("version mismatch"));
        }

        Ok::<_, io::Error>(hello)
    })
    .await;

    match res {
        Ok(Ok(hello)) => {
            let _ = ctrl_tx.send(ControlMsg::Hello { stream, hello });
        }
        Ok(Err(e)) => eprintln!("[mn-srv] intake error: {e}"),
        Err(_) => eprintln!("[mn-srv] intake timed out"),
    }
}

// ---------------------------------------------------------------------------
// Per-client writer task
// ---------------------------------------------------------------------------

async fn writer_task(
    id: u64,
    mut w: OwnedWriteHalf,
    snapshot: Vec<u8>,
    mut rx: mpsc::Receiver<OutMsg>,
    ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
    capacity_changed: std::sync::Arc<Notify>,
) {
    // 1. Send replay snapshot.
    let mut ok = true;
    if !snapshot.is_empty() {
        for chunk in snapshot.chunks(MAX_PAYLOAD) {
            let pkt = Packet::replay(chunk);
            if send_packet(&mut w, &pkt).await.is_err() {
                ok = false;
                break;
            }
        }
    }
    if ok
        && send_packet(&mut w, &Packet::empty(MsgType::ReplayEnd))
            .await
            .is_err()
    {
        ok = false;
    }
    if !ok {
        drop(rx);
        capacity_changed.notify_one();
        let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
        return;
    }
    // 2. Notify main: client is Live. Queue draining does not require an ack.
    if ctrl_tx.send(ControlMsg::ClientLive { id }).is_err() {
        return;
    }
    // 3. Drain outbound channel.
    let mut final_sent = false;
    while let Some(msg) = rx.recv().await {
        // notify_one stores a permit if mainloop is between its capacity
        // check and select, so a freed slot cannot cause a lost wakeup.
        capacity_changed.notify_one();
        match msg {
            OutMsg::Packet(p) => {
                if send_packet(&mut w, &p).await.is_err() {
                    drop(rx);
                    capacity_changed.notify_one();
                    let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
                    return;
                }
                final_sent |= matches!(p.msg_type, MsgType::Exit | MsgType::Error);
            }
            OutMsg::Shutdown => break,
        }
    }
    // Graceful close: shut the write half so the client sees EOF.
    if w.shutdown().await.is_ok() && final_sent {
        let _ = ctrl_tx.send(ControlMsg::ClientFinished);
    }
    let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
}

// ---------------------------------------------------------------------------
// Per-client reader task
// ---------------------------------------------------------------------------

async fn reader_task(
    id: u64,
    mut r: OwnedReadHalf,
    ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
    leader: std::sync::Arc<AsyncFd<OwnedFd>>,
    pty_write_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    flags: ClientFlags,
) {
    let readonly = flags.contains(ClientFlags::READONLY);
    loop {
        let pkt = match recv_packet(&mut r).await {
            Ok(p) => p,
            Err(_) => break,
        };
        let send_ok = match pkt.msg_type {
            MsgType::Content => {
                // Hot path: write straight to the PTY leader, skipping
                // the main task. The Mutex serializes against other
                // reader tasks (main never writes the PTY).
                if !readonly {
                    let _g = pty_write_lock.lock().await;
                    write_pty(&leader, &pkt.payload).await;
                }
                true
            }
            MsgType::Resize => match pkt.parse_resize() {
                Some((rows, cols)) => ctrl_tx.send(ControlMsg::Resize { id, rows, cols }).is_ok(),
                None => true,
            },
            MsgType::Detach => {
                let _ = ctrl_tx.send(ControlMsg::Detach { id });
                break;
            }
            _ => true, // ignore unexpected messages
        };
        if !send_ok {
            break;
        }
    }
    let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
}

#[cfg(test)]
#[path = "server_exit_tests.rs"]
mod exit_tests;

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    #[test]
    fn pty_descriptors_are_close_on_exec() {
        let (leader, follower) = open_pty().unwrap();
        for fd in [&leader, &follower] {
            assert!(
                rustix::io::fcntl_getfd(fd)
                    .unwrap()
                    .contains(rustix::io::FdFlags::CLOEXEC),
                "PTY descriptors must not leak into execed commands"
            );
        }
    }

    #[test]
    fn internal_server_rejects_invalid_capacity_and_reports_it() {
        for size in [0, crate::startup::MAX_RING_SIZE + 1] {
            let dir = tempfile::TempDir::new().unwrap();
            let socket = dir.path().join("startup");
            let (reader, writer) = rustix::pipe::pipe().unwrap();
            // Transfer ownership as the real CLI does across exec.
            let args = vec![
                "startup".into(),
                "--ready-fd".into(),
                writer.into_raw_fd().to_string(),
                "--ring-size".into(),
                size.to_string(),
                "--socket-path".into(),
                socket.to_string_lossy().into_owned(),
                "--".into(),
                "/usr/bin/true".into(),
            ];
            let result = run_server(&args);
            assert!(result.is_err());
            let err = crate::startup::wait(&reader, Duration::from_secs(1)).unwrap_err();
            assert!(err.to_string().contains("ring size must be between"));
            assert!(!socket.exists());
            assert!(!dir.path().join("startup.lock").exists());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::pin::Pin;
    use std::task::Poll;
    use tokio::sync::oneshot;

    async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
    }

    fn client(id: u64, state: ClientState, out_tx: mpsc::Sender<OutMsg>) -> ClientHandle {
        ClientHandle {
            id,
            flags: ClientFlags::empty(),
            state,
            is_controller: false,
            out_tx,
            resize_pending: false,
            terminal_pending: None,
            shutdown_pending: false,
            drain_deadline: None,
            tasks: Vec::new(),
        }
    }

    #[tokio::test]
    async fn full_queue_defers_coalesced_controls_without_blocking() {
        let (out_tx, mut out_rx) = mpsc::channel(2);
        let mut clients = vec![client(1, ClientState::Live, out_tx)];
        for _ in 0..2 {
            clients[0]
                .out_tx
                .try_send(OutMsg::Packet(Packet::content(b"queued")))
                .unwrap();
        }
        elect_controller(&mut clients);
        clients[0].terminal_pending = Some(Packet::exit(7));
        clients[0].shutdown_pending = true;
        clients[0].flush_controls();
        assert!(clients[0].is_controller);
        assert!(clients[0].controls_pending());
        assert!(!output_ready(&clients));
        for expected in [
            MsgType::Content,
            MsgType::Content,
            MsgType::ResizeReq,
            MsgType::Exit,
        ] {
            clients[0].flush_controls();
            let OutMsg::Packet(packet) = out_rx.recv().await.unwrap() else {
                panic!("Shutdown overtook queued output/control");
            };
            assert_eq!(packet.msg_type, expected);
        }
        clients[0].flush_controls();
        assert!(matches!(out_rx.recv().await, Some(OutMsg::Shutdown)));
        assert!(!clients[0].controls_pending());
    }

    #[tokio::test]
    async fn blocked_replay_queues_ordered_output_with_bounded_backpressure() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
            rustix::net::sockopt::set_socket_send_buffer_size(&server, 4096).unwrap();
            server.writable().await.unwrap();
            let (_read, write) = server.into_split();
            let snapshot: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAP);
            let (live_tx, mut live_rx) = mpsc::channel(OUTBOUND_CAP);
            let clients = vec![
                client(1, ClientState::Replaying, out_tx.clone()),
                client(2, ClientState::Live, live_tx),
            ];
            let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
            let capacity_changed = std::sync::Arc::new(Notify::new());
            // Disable cooperative yields only in this test, so Pending means
            // socket/channel backpressure rather than an exhausted task budget.
            let mut writer = Box::pin(tokio::task::unconstrained(writer_task(
                1,
                write,
                snapshot.clone(),
                out_rx,
                ctrl_tx,
                capacity_changed.clone(),
            )));

            // No peer reads yet: replay cannot finish. Keep the writer parked
            // while a separately gated producer emits post-snapshot output.
            assert!(poll_once(writer.as_mut()).await.is_pending());
            assert!(matches!(
                ctrl_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            let expected: Vec<u8> = (0..OUTBOUND_CAP * 2 + 3)
                .flat_map(|i| {
                    let mut chunk = vec![b'.'; MAX_PAYLOAD];
                    let marker = format!("SEQ:{i:04}:");
                    chunk[..marker.len()].copy_from_slice(marker.as_bytes());
                    chunk
                })
                .collect();
            let (release, gate) = oneshot::channel();
            let mut producer = Box::pin(tokio::task::unconstrained(async {
                gate.await.unwrap();
                fan_out(&clients, &expected, &capacity_changed).await;
                for c in &clients {
                    c.out_tx.send(OutMsg::Shutdown).await.unwrap();
                }
            }));
            assert!(poll_once(producer.as_mut()).await.is_pending());
            assert_eq!(out_tx.capacity(), OUTBOUND_CAP);
            release.send(()).unwrap();
            assert!(poll_once(producer.as_mut()).await.is_pending());
            // The next read is blocked, not dropped or buffered elsewhere.
            assert_eq!(out_tx.capacity(), 0);
            assert_eq!(clients[1].out_tx.capacity(), 0);
            assert!(matches!(
                ctrl_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));

            let receive = async {
                let mut replay = Vec::new();
                let mut content = Vec::new();
                let mut replay_ended = false;
                loop {
                    let pkt = match recv_packet(&mut peer).await {
                        Ok(pkt) => pkt,
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                        Err(e) => panic!("read failed: {e}"),
                    };
                    match pkt.msg_type {
                        MsgType::Replay => {
                            assert!(!replay_ended, "Replay after ReplayEnd");
                            replay.extend_from_slice(&pkt.payload);
                        }
                        MsgType::ReplayEnd => {
                            assert!(!replay_ended, "duplicate ReplayEnd");
                            replay_ended = true;
                        }
                        MsgType::Content => {
                            assert!(replay_ended, "Content before ReplayEnd");
                            content.extend_from_slice(&pkt.payload);
                        }
                        other => panic!("unexpected packet: {other:?}"),
                    }
                }
                assert!(replay_ended);
                assert_eq!(replay, snapshot);
                assert_eq!(content, expected);
            };
            let receive_live = async {
                let mut content = Vec::new();
                while let Some(msg) = live_rx.recv().await {
                    capacity_changed.notify_one();
                    match msg {
                        OutMsg::Packet(pkt) => {
                            assert_eq!(pkt.msg_type, MsgType::Content);
                            content.extend_from_slice(&pkt.payload);
                        }
                        OutMsg::Shutdown => break,
                    }
                }
                assert_eq!(content, expected);
            };
            // Deliberately do not process ClientLive. Queue draining must
            // progress while PTY admission is paused.
            tokio::join!(writer, producer, receive, receive_live);
            assert!(matches!(
                ctrl_rx.recv().await,
                Some(ControlMsg::ClientLive { id: 1 })
            ));
            assert!(matches!(
                ctrl_rx.recv().await,
                Some(ControlMsg::ClientGone { id: 1 })
            ));
        })
        .await
        .expect("blocked replay did not resume before deadline");
    }

    #[tokio::test]
    async fn disconnected_replay_releases_backpressure_for_other_clients() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (server, peer) = tokio::net::UnixStream::pair().unwrap();
            rustix::net::sockopt::set_socket_send_buffer_size(&server, 4096).unwrap();
            server.writable().await.unwrap();
            let (_read, write) = server.into_split();
            let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAP);
            let (live_tx, mut live_rx) = mpsc::channel(OUTBOUND_CAP);
            let clients = vec![
                client(1, ClientState::Replaying, out_tx.clone()),
                client(2, ClientState::Live, live_tx),
            ];
            let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
            let capacity_changed = std::sync::Arc::new(Notify::new());
            let mut writer = Box::pin(tokio::task::unconstrained(writer_task(
                1,
                write,
                vec![b'R'; 1024 * 1024],
                out_rx,
                ctrl_tx,
                capacity_changed.clone(),
            )));
            assert!(poll_once(writer.as_mut()).await.is_pending());
            let expected: Vec<u8> = (0..OUTBOUND_CAP + 2)
                .flat_map(|i| {
                    let mut chunk = vec![b'.'; MAX_PAYLOAD];
                    let marker = format!("SEQ:{i:04}:");
                    chunk[..marker.len()].copy_from_slice(marker.as_bytes());
                    chunk
                })
                .collect();
            let mut producer = Box::pin(tokio::task::unconstrained(async {
                fan_out(&clients, &expected, &capacity_changed).await;
                clients[1].out_tx.send(OutMsg::Shutdown).await.unwrap();
            }));
            assert!(poll_once(producer.as_mut()).await.is_pending());
            assert_eq!(out_tx.capacity(), 0);

            // A replay write failure must drop its receiver and unblock the
            // pending send, even before mainloop processes ClientGone.
            drop(peer);
            let receive_live = async {
                let mut content = Vec::new();
                while let Some(msg) = live_rx.recv().await {
                    capacity_changed.notify_one();
                    match msg {
                        OutMsg::Packet(pkt) => {
                            assert_eq!(pkt.msg_type, MsgType::Content);
                            content.extend_from_slice(&pkt.payload);
                        }
                        OutMsg::Shutdown => break,
                    }
                }
                assert_eq!(content, expected);
            };
            tokio::join!(writer, producer, receive_live);
            assert!(out_tx.is_closed());
            assert!(matches!(
                ctrl_rx.recv().await,
                Some(ControlMsg::ClientGone { id: 1 })
            ));
            assert!(ctrl_rx.recv().await.is_none());
        })
        .await
        .expect("disconnected replay did not release backpressure before deadline");
    }

    #[tokio::test]
    async fn output_queued_before_live_event_is_processed_follows_empty_replay() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let (_read, write) = server.into_split();
            let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAP);
            let clients = vec![client(1, ClientState::Replaying, out_tx)];
            let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
            let capacity_changed = std::sync::Arc::new(Notify::new());
            let writer = tokio::spawn(writer_task(
                1,
                write,
                Vec::new(),
                out_rx,
                ctrl_tx,
                capacity_changed.clone(),
            ));
            // ReplayEnd is on the wire, but mainloop has not handled ClientLive.
            assert_eq!(
                recv_packet(&mut peer).await.unwrap().msg_type,
                MsgType::ReplayEnd
            );
            assert!(matches!(
                ctrl_rx.recv().await,
                Some(ControlMsg::ClientLive { id: 1 })
            ));
            assert_eq!(clients[0].state, ClientState::Replaying);
            fan_out(&clients, b"SEQ:0000:SEQ:0001:", &capacity_changed).await;
            clients[0].out_tx.send(OutMsg::Shutdown).await.unwrap();
            writer.await.unwrap();
            let pkt = recv_packet(&mut peer).await.unwrap();
            assert_eq!(pkt.msg_type, MsgType::Content);
            assert_eq!(pkt.payload, b"SEQ:0000:SEQ:0001:");
            assert_eq!(
                recv_packet(&mut peer).await.unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        })
        .await
        .expect("empty replay handoff did not complete before deadline");
    }
}
