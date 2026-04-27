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
//!    packet, validates protocol version + peer UID, sends `Welcome`.
//!    On `Query` intent, exits. On `Attach`, forwards
//!    `ControlMsg::Register{stream, hello}` to the main task.
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
//! Backpressure: PTY fan-out only sends to *Live* clients. The bounded
//! per-client outbound channel means a slow client makes the main task
//! `await` on `send`, which stalls the PTY read loop and ultimately the
//! child — same semantics as the original poll-based server.

use crate::protocol::{
    self, ClientFlags, HEADER_SIZE, Hello, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
};
use crate::ring::RingBuffer;

use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Capacity of each client's outbound channel, in packets. Each `Content`
/// packet is at most `HEADER_SIZE + MAX_PAYLOAD` ≈ 4 KiB. 64 packets ≈
/// 256 KiB — matches the original byte-budget per client.
const OUTBOUND_CAP: usize = 64;
const READ_BUF_SIZE: usize = 4096;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

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
    unsafe { Ok((OwnedFd::from_raw_fd(leader), OwnedFd::from_raw_fd(follower))) }
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

    let ready_fd = unsafe { OwnedFd::from_raw_fd(opts.ready_fd) };
    rustix::io::fcntl_setfd(&ready_fd, rustix::io::FdFlags::CLOEXEC)?;

    // Detach from controlling terminal. setsid may fail if we're already a
    // process group leader — that's fine.
    let _ = rustix::process::setsid();
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // Acquire the session lock BEFORE binding — kernel releases on death.
    let lock_path = opts.socket_path.with_file_name(format!(
        "{}.lock",
        opts.socket_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("invalid socket path: {}", opts.socket_path.display()))?
    ));
    let _lock_fd = match crate::socket::try_acquire_lock(&lock_path)? {
        Some(fd) => fd,
        None => {
            return Err(format!(
                "another server already holds the session lock: {}",
                lock_path.display()
            )
            .into());
        }
    };

    // Bind + listen BEFORE spawning the child — fail early.
    let std_listener = bind_listen(&opts.socket_path)
        .map_err(|e| format!("bind {}: {e}", opts.socket_path.display()))?;
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
        // Spawn the child *inside* the runtime so tokio can wire SIGCHLD.
        let mut child = spawn_child(&opts.command, follower_fd)
            .map_err(|e| format!("spawn {:?}: {e}", opts.command))?;
        let child_pid = child.id().unwrap_or(0);

        // Signal readiness to the CLI by closing the pipe.
        drop(ready_fd);

        // PTY leader must be non-blocking for AsyncFd.
        set_nonblocking(&leader_fd)?;
        let leader = std::sync::Arc::new(
            AsyncFd::with_interest(leader_fd, Interest::READABLE | Interest::WRITABLE)
                .map_err(|e| format!("AsyncFd PTY: {e}"))?,
        );

        let listener = UnixListener::from_std(std_listener)?;

        let result = mainloop(listener, leader, &mut child, child_pid, opts.ring_size).await;

        // Make sure the child is reaped before we exit.
        if let Ok(None) = child.try_wait() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        result
    })?;

    let _ = std::fs::remove_file(&opts.socket_path);
    let _ = std::fs::remove_file(&lock_path);
    eprintln!("[mn-srv] exit clean");
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket binding
// ---------------------------------------------------------------------------

fn bind_listen(path: &std::path::Path) -> io::Result<std::os::unix::net::UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
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
            libc::setsid();
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn()
}

// ---------------------------------------------------------------------------
// Server-side state and messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Replaying, // writer hasn't finished sending the snapshot yet
    Live,      // bidirectional; included in PTY fan-out
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
}

enum ControlMsg {
    /// Intake task finished handshake and the client wants to attach.
    Register {
        stream: tokio::net::UnixStream,
        hello: Hello,
    },
    /// Writer task finished sending the replay; client transitions to Live.
    ClientLive { id: u64 },
    /// Reader task: client requested a resize.
    Resize { id: u64, rows: u16, cols: u16 },
    /// Reader task: client sent Detach (close gracefully).
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
    child_pid: u32,
    ring_size: usize,
) -> Result<(), crate::Error> {
    let mut ring = RingBuffer::new(ring_size);
    let mut clients: Vec<ClientHandle> = Vec::new();
    let mut next_id: u64 = 1;
    let mut child_running = true;
    let mut exit_status: Option<u32> = None;
    let mut exit_notified = false;
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
    let pty_write_lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut read_buf = vec![0u8; READ_BUF_SIZE];

    loop {
        // Termination condition: child gone, every client notified+drained.
        if !child_running && clients.is_empty() && exit_notified {
            eprintln!("[mn-srv] mainloop exit: child_gone+no_clients+notified");
            break;
        }

        // PTY readability is only "interesting" while child is running.
        // To avoid duck-finding #4 (replay clients shouldn't gate PTY),
        // we always read PTY when child is alive; fan-out only goes to
        // Live clients.
        tokio::select! {
            biased;

            // Child exit ------------------------------------------------
            res = child.wait(), if child_running => {
                child_running = false;
                let status = match res {
                    Ok(s) => s.code().unwrap_or(1) as u32,
                    Err(_) => 1,
                };
                exit_status = Some(status);
                eprintln!("[mn-srv] child exited status={status} live_clients={}", clients.len());
                let pkt = Packet::exit(status);
                for c in &clients {
                    if c.state == ClientState::Live {
                        // Best-effort send; if writer is gone the channel is closed.
                        let _ = c.out_tx.send(OutMsg::Packet(pkt.clone())).await;
                        exit_notified = true;
                    }
                }
                // Replaying clients will receive Exit when they transition
                // to Live (see ClientLive handler). If there are zero
                // clients at all, we KEEP the server running so a future
                // attach can pick up the exit status — same behavior as
                // the original poll-based server.
            }

            // PTY readable ----------------------------------------------
            // Note: we keep polling even after `child_running` flips false,
            // because data the child wrote just before exiting may still be
            // sitting in the PTY master read buffer. We only stop on EIO/EOF.
            guard = leader.readable(), if !pty_eof => {
                let mut guard = match guard {
                    Ok(g) => g,
                    Err(_) => continue,
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

                        // Fan out to Live clients (chunked to MAX_PAYLOAD).
                        // Backpressure: if a Live client's bounded mpsc is
                        // full, this `await` blocks the PTY read loop.
                        // That stalls the child via the kernel PTY buffer.
                        for chunk in data.chunks(MAX_PAYLOAD) {
                            let pkt = Packet::content(chunk);
                            for c in &clients {
                                if c.state == ClientState::Live
                                    && c.out_tx.send(OutMsg::Packet(pkt.clone())).await.is_err()
                                {
                                    // writer task gone; will be cleaned
                                    // up via ClientGone shortly.
                                }
                            }
                        }
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
                            eprintln!("[mn-srv] pty read err: {e}");
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
                        tokio::spawn(intake_task(stream, ctx, child_running, exit_status, ring.capacity() as u32, ring.len() as u32));
                    }
                    Err(e) => eprintln!("[mn-srv] accept: {e}"),
                }
            }

            // Control events from intake / reader / writer tasks --------
            Some(msg) = ctrl_rx.recv() => {
                match msg {
                    ControlMsg::Register { stream, hello } => {
                        let id = next_id; next_id += 1;
                        let snapshot = ring.snapshot();
                        let (out_tx, out_rx) = mpsc::channel::<OutMsg>(OUTBOUND_CAP);
                        let (read_half, write_half) = stream.into_split();
                        clients.push(ClientHandle {
                            id,
                            flags: hello.flags,
                            state: ClientState::Replaying,
                            is_controller: false,
                            out_tx,
                        });
                        tokio::spawn(writer_task(id, write_half, snapshot, out_rx, ctrl_tx.clone()));
                        tokio::spawn(reader_task(id, read_half, ctrl_tx.clone(), leader.clone(), pty_write_lock.clone(), hello.flags));
                    }
                    ControlMsg::ClientLive { id } => {
                        if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                            c.state = ClientState::Live;
                            // If child already exited before this client became
                            // Live, send Exit immediately so it doesn't hang.
                            if !child_running && let Some(s) = exit_status {
                                let _ = c.out_tx.send(OutMsg::Packet(Packet::exit(s))).await;
                                let _ = c.out_tx.send(OutMsg::Shutdown).await;
                                exit_notified = true;
                            }
                            elect_controller(&mut clients).await;
                        }
                    }
                    ControlMsg::Resize { id, rows, cols } => {
                        let is_ctrl = clients.iter().find(|c| c.id == id).map(|c| c.is_controller).unwrap_or(false);
                        if is_ctrl {
                            let ws = rustix::termios::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                            let _ = rustix::termios::tcsetwinsize(leader.get_ref(), ws);
                            if let Some(pgid) = rustix::process::Pid::from_raw(child_pid as i32) {
                                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::WINCH);
                            }
                        }
                    }
                    ControlMsg::Detach { id } => {
                        if let Some(c) = clients.iter().find(|c| c.id == id) {
                            let _ = c.out_tx.send(OutMsg::Shutdown).await;
                        }
                    }
                    ControlMsg::ClientGone { id } => {
                        if let Some(pos) = clients.iter().position(|c| c.id == id) {
                            clients.remove(pos);
                            elect_controller(&mut clients).await;
                        }
                    }
                }
            }
        }
    }

    Ok(())
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

async fn elect_controller(clients: &mut [ClientHandle]) {
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
        let pkt = Packet::empty(MsgType::ResizeReq);
        let _ = clients[i].out_tx.send(OutMsg::Packet(pkt)).await;
    }
}

// ---------------------------------------------------------------------------
// Intake task — reads Hello, writes Welcome
// ---------------------------------------------------------------------------

async fn intake_task(
    mut stream: tokio::net::UnixStream,
    ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
    child_running: bool,
    exit_status: Option<u32>,
    ring_size: u32,
    ring_used: u32,
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

        let welcome = protocol::Welcome {
            version: PROTOCOL_VERSION,
            server_pid: std::process::id(),
            child_pid: 0,
            child_running,
            exit_status: exit_status.unwrap_or(0) as u8,
            client_count: 0,
            ring_size,
            ring_used,
        };
        send_packet(&mut stream, &Packet::welcome(&welcome)).await?;

        Ok::<_, io::Error>(hello)
    })
    .await;

    match res {
        Ok(Ok(hello)) => match hello.intent {
            Intent::Query => {
                // Drop after Welcome.
            }
            Intent::Attach => {
                let _ = ctrl_tx.send(ControlMsg::Register { stream, hello });
            }
        },
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
) {
    // 1. Send replay snapshot.
    let mut ok = true;
    if !snapshot.is_empty() {
        for chunk in snapshot.chunks(MAX_PAYLOAD) {
            let pkt = Packet::replay(chunk);
            if write_pkt(&mut w, &pkt).await.is_err() {
                ok = false;
                break;
            }
        }
    }
    if ok
        && write_pkt(&mut w, &Packet::empty(MsgType::ReplayEnd))
            .await
            .is_err()
    {
        ok = false;
    }
    if !ok {
        let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
        return;
    }
    // 2. Notify main: client is Live.
    if ctrl_tx.send(ControlMsg::ClientLive { id }).is_err() {
        return;
    }
    // 3. Drain outbound channel.
    while let Some(msg) = rx.recv().await {
        match msg {
            OutMsg::Packet(p) => {
                if write_pkt(&mut w, &p).await.is_err() {
                    let _ = ctrl_tx.send(ControlMsg::ClientGone { id });
                    return;
                }
            }
            OutMsg::Shutdown => break,
        }
    }
    // Graceful close: shut the write half so the client sees EOF.
    let _ = w.shutdown().await;
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
        let pkt = match recv_packet_half(&mut r).await {
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

// ---------------------------------------------------------------------------
// Async packet I/O helpers (cancellation-safe variants are NOT provided —
// callers must own their stream and not race these inside select!).
// ---------------------------------------------------------------------------

async fn send_packet(stream: &mut tokio::net::UnixStream, pkt: &Packet) -> io::Result<()> {
    let buf = pkt.encode();
    stream.write_all(&buf).await
}

async fn recv_packet(stream: &mut tokio::net::UnixStream) -> io::Result<Packet> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let msg_type = MsgType::from_u8(header[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown message type"))?;
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok(Packet::new(msg_type, payload))
}

async fn write_pkt(w: &mut OwnedWriteHalf, pkt: &Packet) -> io::Result<()> {
    let buf = pkt.encode();
    w.write_all(&buf).await
}

async fn recv_packet_half(r: &mut OwnedReadHalf) -> io::Result<Packet> {
    let mut header = [0u8; HEADER_SIZE];
    r.read_exact(&mut header).await?;
    let msg_type = MsgType::from_u8(header[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown message type"))?;
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Packet::new(msg_type, payload))
}
