use crate::protocol::{self, ClientFlags, Intent, MsgType, PROTOCOL_VERSION, Packet};
use crate::ring::RingBuffer;

use rustix::event::{PollFd, PollFlags};
use std::collections::VecDeque;
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OUTBOUND_LIMIT: usize = 256 * 1024; // 256 KiB per client
const READ_BUF_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

/// Create a pipe with both ends set to non-blocking.
fn pipe_nonblock() -> io::Result<(OwnedFd, OwnedFd)> {
    let (r, w) = rustix::pipe::pipe()?;
    set_nonblocking(&r)?;
    set_nonblocking(&w)?;
    Ok((r, w))
}

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

/// Check that the peer on the other end of a Unix socket is the same UID as us.
/// Returns Ok(()) if the peer matches, Err if not or if the check fails.
#[cfg(target_os = "macos")]
fn verify_peer_uid(fd: &OwnedFd) -> io::Result<()> {
    let my_uid = rustix::process::getuid().as_raw();
    let mut peer_uid: libc::uid_t = 0;
    let mut peer_gid: libc::gid_t = 0;
    let ret = unsafe { libc::getpeereid(fd.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
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
fn verify_peer_uid(fd: &OwnedFd) -> io::Result<()> {
    let my_uid = rustix::process::getuid().as_raw();
    let cred = rustix::net::sockopt::socket_peercred(fd)?;
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
fn verify_peer_uid(_fd: &OwnedFd) -> io::Result<()> {
    // No peer credential check available on this platform
    Ok(())
}

// ---------------------------------------------------------------------------
// Client state on the server side
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Connected, // Hello not yet received
    Replaying, // Sending replay data
    Live,      // Bidirectional
}

struct ServerClient {
    fd: OwnedFd,
    state: ClientState,
    flags: ClientFlags,
    outbound: VecDeque<u8>,
    replay_buf: Option<Vec<u8>>, // snapshot being sent
    replay_pos: usize,           // position in replay_buf
    is_controller: bool,
}

impl ServerClient {
    fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            state: ClientState::Connected,
            flags: ClientFlags::empty(),
            outbound: VecDeque::new(),
            replay_buf: None,
            replay_pos: 0,
            is_controller: false,
        }
    }

    /// Queue a packet for sending. Returns false if the outbound buffer
    /// is full (client should be disconnected).
    fn queue_packet(&mut self, pkt: &Packet) -> bool {
        let encoded = pkt.encode();
        if self.outbound.len() + encoded.len() > OUTBOUND_LIMIT {
            return false; // slow client
        }
        self.outbound.extend(encoded.iter());
        true
    }

    /// Returns true if `extra` more bytes would fit in outbound without
    /// exceeding the per-client limit. Used by the replay path to apply
    /// backpressure instead of dropping the client.
    fn outbound_has_room(&self, extra: usize) -> bool {
        self.outbound.len() + extra <= OUTBOUND_LIMIT
    }

    /// Try to flush the outbound buffer. Returns false on fatal error.
    fn flush(&mut self) -> bool {
        while !self.outbound.is_empty() {
            let (front, back) = self.outbound.as_slices();
            let buf = if !front.is_empty() { front } else { back };
            match protocol::try_write(self.fd.as_fd(), buf) {
                protocol::WriteResult::Complete => {
                    let len = buf.len();
                    self.outbound.drain(..len);
                }
                protocol::WriteResult::Partial(n) => {
                    self.outbound.drain(..n);
                    return true; // can't write more right now
                }
                protocol::WriteResult::WouldBlock => return true,
                protocol::WriteResult::Error => return false,
            }
        }
        true
    }

    fn has_pending_output(&self) -> bool {
        !self.outbound.is_empty() || self.replay_buf.is_some()
    }
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
            "--" => {
                past_separator = true;
            }
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
            other => {
                return Err(arg_err(format!("unknown server option: {other}")));
            }
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
// Server entry point
// ---------------------------------------------------------------------------

pub fn run_server(args: &[String]) -> Result<(), crate::Error> {
    let opts = parse_server_args(args)?;

    // Take ownership of the ready fd and set CLOEXEC so the child doesn't inherit it
    let ready_fd = unsafe { OwnedFd::from_raw_fd(opts.ready_fd) };
    rustix::io::fcntl_setfd(&ready_fd, rustix::io::FdFlags::CLOEXEC)?;

    // Fully detach from the calling session.
    // Note: setsid() may fail if we're already a process group leader
    // (which happens with process_group(0)). That's OK — the important
    // thing is that stdio is /dev/null and we're in a new process group.
    let _ = rustix::process::setsid();

    // Ignore SIGHUP as defense in depth; ignore SIGPIPE so that writes
    // to a PTY leader whose follower has closed (or to a client socket
    // whose peer has gone away) return EPIPE instead of killing the
    // server. Without this, the next stray client byte forwarded to a
    // dead child kills the server before it can send Exit, and clients
    // see a mysterious "unexpected end of file".
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Acquire the session lock BEFORE binding. The lock file lives next
    // to the socket and is flocked for this process's lifetime. The
    // kernel releases it on any form of process death, which is how we
    // detect stale sessions. We hold `_lock_fd` in scope until the end
    // of the function.
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

    // Bind + listen BEFORE spawning child — fail early
    let listener = bind_listen(&opts.socket_path)
        .map_err(|e| format!("bind {}: {e}", opts.socket_path.display()))?;
    listener.set_nonblocking(true)?;

    // Open PTY
    let (leader_fd, follower_fd) = open_pty().map_err(|e| format!("openpty: {e}"))?;

    // Set initial terminal size on the PTY
    let winsize = rustix::termios::Winsize {
        ws_row: opts.rows,
        ws_col: opts.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = rustix::termios::tcsetwinsize(&leader_fd, winsize);

    // Spawn child
    let child = spawn_child(&opts.command, follower_fd)
        .map_err(|e| format!("spawn {:?}: {e}", opts.command))?;
    let child_pid = child.id();

    // Signal readiness to the CLI by closing the ready fd
    drop(ready_fd);

    // Set leader_fd to non-blocking
    set_nonblocking(&leader_fd)?;

    // Run the event loop
    server_mainloop(
        listener,
        leader_fd,
        child,
        child_pid,
        opts.ring_size,
        &opts.socket_path,
    )?;

    // Cleanup
    let _ = std::fs::remove_file(&opts.socket_path);
    let _ = std::fs::remove_file(&lock_path);
    // _lock_fd drops here, releasing the flock (though the file is already gone)
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket binding
// ---------------------------------------------------------------------------

fn bind_listen(path: &std::path::Path) -> io::Result<UnixListener> {
    // Remove stale socket if present
    if path.exists() {
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;

    // Set socket permissions to 0600
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    Ok(listener)
}

// ---------------------------------------------------------------------------
// Child spawning
// ---------------------------------------------------------------------------

fn spawn_child(command: &[String], follower_fd: OwnedFd) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let (cmd, args) = if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        (shell, Vec::new())
    } else {
        (command[0].clone(), command[1..].to_vec())
    };

    let follower_for_stdout = follower_fd.try_clone()?;
    let follower_for_stderr = follower_fd.try_clone()?;

    let mut command = Command::new(&cmd);
    command
        .args(&args)
        .stdin(Stdio::from(follower_fd))
        .stdout(Stdio::from(follower_for_stdout))
        .stderr(Stdio::from(follower_for_stderr));

    unsafe {
        command.pre_exec(|| {
            // Create a new session so the PTY becomes the controlling terminal
            libc::setsid();
            // Set the controlling terminal
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn()
}

// ---------------------------------------------------------------------------
// Server main loop
// ---------------------------------------------------------------------------

fn server_mainloop(
    listener: UnixListener,
    leader_fd: OwnedFd,
    mut child: std::process::Child,
    child_pid: u32,
    ring_size: usize,
    _socket_path: &std::path::Path,
) -> io::Result<()> {
    let mut ring = RingBuffer::new(ring_size);
    let mut clients: Vec<ServerClient> = Vec::new();
    let mut child_running = true;
    let mut exit_status: Option<u32> = None;
    let mut exit_notified = false; // at least one client saw the exit

    // Set up signal self-pipe for SIGCHLD
    let (sig_read, sig_write) = pipe_nonblock()?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, sig_write)?;

    let mut read_buf = [0u8; READ_BUF_SIZE];

    loop {
        // Build poll fds — collect raw fds to avoid borrow issues
        let mut pollfds: Vec<PollFd<'_>> = Vec::new();

        // [0] = listener
        pollfds.push(PollFd::new(&listener, PollFlags::IN));
        // [1] = PTY leader (only read if child is running)
        let pty_flags = if child_running {
            PollFlags::IN
        } else {
            PollFlags::empty()
        };
        pollfds.push(PollFd::new(&leader_fd, pty_flags));
        // [2] = signal pipe
        pollfds.push(PollFd::new(&sig_read, PollFlags::IN));
        // [3..] = clients
        for c in &clients {
            let mut flags = PollFlags::IN;
            if c.has_pending_output() {
                flags |= PollFlags::OUT;
            }
            pollfds.push(PollFd::new(&c.fd, flags));
        }

        // poll with no timeout (block indefinitely)
        match rustix::event::poll(&mut pollfds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }

        // Extract revents before dropping pollfds (which borrow clients)
        let revents: Vec<PollFlags> = pollfds.iter().map(|p| p.revents()).collect();
        drop(pollfds);

        // Process signals
        if revents[2].contains(PollFlags::IN) {
            // Drain the signal pipe
            let mut sig_buf = [0u8; 64];
            let _ = rustix::io::read(&sig_read, &mut sig_buf);

            // Check if child exited
            if child_running {
                handle_child_exit(
                    &mut child,
                    &mut child_running,
                    &mut exit_status,
                    &mut clients,
                    &mut exit_notified,
                );
            }
        }

        // Accept new clients
        if revents[0].contains(PollFlags::IN)
            && let Ok((stream, _)) = listener.accept()
        {
            let fd = OwnedFd::from(stream);
            // Verify peer UID before accepting
            if verify_peer_uid(&fd).is_err() {
                drop(fd); // reject connection
            } else {
                let _ = rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK);
                // Cap the kernel send buffer so backpressure is observable
                // in userspace. Without this, Linux's socket-buffer auto-
                // tuning can absorb several MiB before write() returns
                // EAGAIN, defeating the per-client OUTBOUND_LIMIT cap.
                let _ = rustix::net::sockopt::set_socket_send_buffer_size(&fd, 64 * 1024);
                clients.push(ServerClient::new(fd));
            }
        }

        // Read from PTY
        let mut pty_data: Option<Vec<u8>> = None;
        if revents[1].intersects(PollFlags::IN | PollFlags::HUP) {
            match protocol::try_read(leader_fd.as_fd(), &mut read_buf) {
                Ok(0) => {} // would block
                Ok(n) => {
                    let data = read_buf[..n].to_vec();
                    ring.write(&data);
                    pty_data = Some(data);
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // PTY closed — child exited
                    handle_child_exit(
                        &mut child,
                        &mut child_running,
                        &mut exit_status,
                        &mut clients,
                        &mut exit_notified,
                    );
                }
                Err(e) => {
                    // EIO is common on macOS when child exits
                    if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) {
                        handle_child_exit(
                            &mut child,
                            &mut child_running,
                            &mut exit_status,
                            &mut clients,
                            &mut exit_notified,
                        );
                    }
                    // else: ignore transient errors
                }
            }
        }

        // Process client I/O
        let mut to_remove: Vec<usize> = Vec::new();

        for (ci, client) in clients.iter_mut().enumerate() {
            let poll_idx = 3 + ci;
            if poll_idx >= revents.len() {
                break;
            }
            let rev = revents[poll_idx];

            // Handle disconnection
            if rev.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
                && !rev.contains(PollFlags::IN)
            {
                to_remove.push(ci);
                continue;
            }

            // Read from client
            if rev.contains(PollFlags::IN) {
                match handle_client_input(
                    client,
                    &leader_fd,
                    child_pid,
                    &ring,
                    child_running,
                    exit_status,
                    &mut exit_notified,
                ) {
                    Ok(true) => {} // continue
                    Ok(false) | Err(_) => {
                        to_remove.push(ci);
                        continue;
                    }
                }
            }

            // Continue replay if in progress
            if client.state == ClientState::Replaying
                && !continue_replay(client, exit_status, &mut exit_notified)
            {
                to_remove.push(ci);
                continue;
            }

            // Send PTY data to live clients (chunked to MAX_PAYLOAD)
            if client.state == ClientState::Live
                && let Some(ref data) = pty_data
            {
                let mut dropped = false;
                for chunk in data.chunks(protocol::MAX_PAYLOAD) {
                    let pkt = Packet::content(chunk);
                    if !client.queue_packet(&pkt) {
                        to_remove.push(ci);
                        dropped = true;
                        break;
                    }
                }
                if dropped {
                    continue;
                }
            }

            // Flush outbound
            if (rev.contains(PollFlags::OUT) || client.has_pending_output()) && !client.flush() {
                to_remove.push(ci);
                continue;
            }
        }

        // Remove disconnected clients, then re-elect controller
        remove_clients(&mut clients, &to_remove);

        // Re-elect controller after any changes
        // (new clients reaching Live, clients removed, etc.)
        let old_controller_id = clients.iter().position(|c| c.is_controller);
        let mut new_controller_id = None;
        for (i, c) in clients.iter().enumerate().rev() {
            if c.state == ClientState::Live
                && !c.flags.contains(ClientFlags::READONLY)
                && !c.flags.contains(ClientFlags::LOW_PRIORITY)
            {
                new_controller_id = Some(i);
                break;
            }
        }
        // If controller changed, update flags and send ResizeReq
        if new_controller_id != old_controller_id {
            for c in clients.iter_mut() {
                c.is_controller = false;
            }
            if let Some(idx) = new_controller_id {
                clients[idx].is_controller = true;
                let pkt = Packet::empty(MsgType::ResizeReq);
                clients[idx].queue_packet(&pkt);
            }
        }

        // Server exit condition: child exited, no clients, at least one was notified
        if !child_running && clients.is_empty() && exit_notified {
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Client input handling
// ---------------------------------------------------------------------------

fn handle_client_input(
    client: &mut ServerClient,
    leader_fd: &OwnedFd,
    child_pid: u32,
    ring: &RingBuffer,
    child_running: bool,
    exit_status: Option<u32>,
    exit_notified: &mut bool,
) -> io::Result<bool> {
    match client.state {
        ClientState::Connected => {
            // Expect a Hello — the fd is non-blocking, so we may get
            // WouldBlock if the client hasn't sent it yet.
            let pkt = match protocol::recv_packet(client.fd.as_fd()) {
                Ok(p) => p,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(_) => return Ok(false),
            };

            if pkt.msg_type != MsgType::Hello {
                let err_pkt = Packet::error("expected Hello");
                let _ = protocol::send_packet(client.fd.as_fd(), &err_pkt);
                return Ok(false);
            }

            let hello = match pkt.parse_hello() {
                Some(h) => h,
                None => return Ok(false),
            };

            // Version check
            if hello.version != PROTOCOL_VERSION {
                let err_pkt = Packet::error(&format!(
                    "protocol version mismatch: client={}, server={}",
                    hello.version, PROTOCOL_VERSION
                ));
                let _ = protocol::send_packet(client.fd.as_fd(), &err_pkt);
                return Ok(false);
            }

            client.flags = hello.flags;

            // Build Welcome
            let welcome = protocol::Welcome {
                version: PROTOCOL_VERSION,
                server_pid: std::process::id(),
                child_pid: 0, // TODO: track properly
                child_running,
                exit_status: exit_status.unwrap_or(0) as u8,
                client_count: 0, // TODO: accurate count
                ring_size: ring.capacity() as u32,
                ring_used: ring.len() as u32,
            };
            let welcome_pkt = Packet::welcome(&welcome);
            if !client.queue_packet(&welcome_pkt) {
                return Ok(false);
            }

            match hello.intent {
                Intent::Query => {
                    // Flush and disconnect
                    client.flush();
                    return Ok(false);
                }
                Intent::Attach => {
                    // Start replay
                    let snapshot = ring.snapshot();
                    if snapshot.is_empty() {
                        // No replay needed
                        let end_pkt = Packet::empty(MsgType::ReplayEnd);
                        if !client.queue_packet(&end_pkt) {
                            return Ok(false);
                        }
                        client.state = ClientState::Live;

                        if let Some(status) = exit_status {
                            let exit_pkt = Packet::exit(status);
                            if !client.queue_packet(&exit_pkt) {
                                return Ok(false);
                            }
                            *exit_notified = true;
                        }

                        if !client.flags.contains(ClientFlags::READONLY)
                            && !client.flags.contains(ClientFlags::LOW_PRIORITY)
                        {
                            // Controller will be elected in the main loop
                        }
                    } else {
                        client.replay_buf = Some(snapshot);
                        client.replay_pos = 0;
                        client.state = ClientState::Replaying;
                    }
                }
            }
        }
        ClientState::Replaying => {
            // Input ignored during replay — just drain the socket
            let mut buf = [0u8; READ_BUF_SIZE];
            let _ = protocol::try_read(client.fd.as_fd(), &mut buf);
        }
        ClientState::Live => {
            let pkt = match protocol::recv_packet(client.fd.as_fd()) {
                Ok(p) => p,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(_) => return Ok(false),
            };

            match pkt.msg_type {
                MsgType::Content if !client.flags.contains(ClientFlags::READONLY) => {
                    let _ = protocol::write_all_fd(leader_fd.as_fd(), &pkt.payload);
                }
                MsgType::Resize => {
                    if client.is_controller
                        && let Some((rows, cols)) = pkt.parse_resize()
                    {
                        let ws = rustix::termios::Winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        let _ = rustix::termios::tcsetwinsize(leader_fd, ws);
                        // Signal the child's process group to redraw.
                        // The child called setsid() so its PGID == its PID.
                        if let Some(pgid) = rustix::process::Pid::from_raw(child_pid as i32) {
                            let _ = rustix::process::kill_process_group(
                                pgid,
                                rustix::process::Signal::WINCH,
                            );
                        }
                    }
                }
                MsgType::Detach => {
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Replay continuation
// ---------------------------------------------------------------------------

fn continue_replay(
    client: &mut ServerClient,
    exit_status: Option<u32>,
    exit_notified: &mut bool,
) -> bool {
    if let Some(ref buf) = client.replay_buf {
        let remaining = buf.len() - client.replay_pos;
        if remaining == 0 {
            // Gate the ReplayEnd (and any follow-on Exit) on outbound
            // having room — same backpressure rule as the chunk loop
            // below. Returning true keeps the client alive; we'll retry
            // next iteration after flush() drains some bytes.
            let end_pkt = Packet::empty(MsgType::ReplayEnd);
            let mut needed = end_pkt.encode().len();
            let exit_pkt = exit_status.map(Packet::exit);
            if let Some(ref p) = exit_pkt {
                needed += p.encode().len();
            }
            if !client.outbound_has_room(needed) {
                return true; // backpressure
            }

            client.replay_buf = None;
            // queue_packet cannot fail here — we just verified room.
            client.queue_packet(&end_pkt);
            client.state = ClientState::Live;

            if let Some(p) = exit_pkt {
                client.queue_packet(&p);
                *exit_notified = true;
            }

            if !client.flags.contains(ClientFlags::READONLY)
                && !client.flags.contains(ClientFlags::LOW_PRIORITY)
            {
                // Controller will be elected in the main loop
            }

            return true;
        }

        let chunk_size = remaining.min(protocol::MAX_PAYLOAD);
        let chunk = &buf[client.replay_pos..client.replay_pos + chunk_size];
        let pkt = Packet::replay(chunk);
        // Backpressure: if outbound doesn't have room for this chunk,
        // skip queueing this iteration. has_pending_output() keeps
        // POLLOUT registered, so we'll be re-polled when the client
        // drains and flush() makes room.
        if !client.outbound_has_room(pkt.encode().len()) {
            return true;
        }
        // queue_packet cannot fail — we just verified room.
        client.queue_packet(&pkt);
        client.replay_pos += chunk_size;
    }

    true
}

fn handle_child_exit(
    child: &mut std::process::Child,
    child_running: &mut bool,
    exit_status: &mut Option<u32>,
    clients: &mut Vec<ServerClient>,
    exit_notified: &mut bool,
) {
    if !*child_running {
        return;
    }
    *child_running = false;
    *exit_status = match child.try_wait() {
        Ok(Some(status)) => Some(status.code().unwrap_or(1) as u32),
        Ok(None) => {
            // Child hasn't been reaped yet (EIO arrived before SIGCHLD).
            // Do a blocking wait — the child is already dead, this returns immediately.
            match child.wait() {
                Ok(status) => Some(status.code().unwrap_or(1) as u32),
                Err(_) => Some(1),
            }
        }
        Err(_) => Some(1),
    };

    // Send Exit to all live clients
    let pkt = Packet::exit(exit_status.unwrap());
    let mut to_remove = Vec::new();
    for (i, c) in clients.iter_mut().enumerate() {
        if c.state == ClientState::Live {
            if !c.queue_packet(&pkt) {
                to_remove.push(i);
            }
            *exit_notified = true;
        }
    }
    remove_clients(clients, &to_remove);
}

// ---------------------------------------------------------------------------
// Controller election
// ---------------------------------------------------------------------------

fn remove_clients(clients: &mut Vec<ServerClient>, indices: &[usize]) {
    if indices.is_empty() {
        return;
    }
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    for &i in sorted.iter().rev() {
        if i < clients.len() {
            clients.remove(i);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ServerClient backed by one end of a socketpair, in Replaying
    /// state with `replay_size` bytes of payload to send.
    fn replaying_client(replay_size: usize) -> (ServerClient, std::os::unix::net::UnixStream) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        // The "server" side: nonblocking, owned by ServerClient.
        a.set_nonblocking(true).unwrap();
        let owned = OwnedFd::from(a);
        let mut c = ServerClient::new(owned);
        c.replay_buf = Some(vec![0xABu8; replay_size]);
        c.state = ClientState::Replaying;
        // Return the peer end so the test holds it open (and can drain).
        (c, b)
    }

    /// Regression: when a slow client lets the server's per-client outbound
    /// queue fill up during replay, `continue_replay` must back-pressure
    /// (return true without queueing) instead of returning false (which the
    /// main loop interprets as "drop this client"). Before the fix, the
    /// server would close brand-new attaches to busy sessions with
    /// "unexpected end of file".
    #[test]
    fn continue_replay_backpressures_when_outbound_full() {
        // Replay larger than OUTBOUND_LIMIT so we must definitely block.
        let (mut client, _peer) = replaying_client(OUTBOUND_LIMIT * 2);
        let mut notified = false;

        // Drive continue_replay repeatedly without ever flushing. With the
        // bug it returned false once outbound exceeded OUTBOUND_LIMIT and
        // the server dropped the client. With the fix it must keep
        // returning true and just stop growing the queue.
        for i in 0..1_000 {
            let alive = continue_replay(&mut client, None, &mut notified);
            assert!(
                alive,
                "continue_replay returned false on iteration {i} \
                 (outbound={}, replay_pos={}) — slow attach would be dropped",
                client.outbound.len(),
                client.replay_pos
            );
            assert!(
                client.outbound.len() <= OUTBOUND_LIMIT,
                "outbound exceeded limit on iteration {i}: {} > {}",
                client.outbound.len(),
                OUTBOUND_LIMIT
            );
        }

        // Sanity: we are still in Replaying with progress made and queue
        // sitting near (but not above) the limit.
        assert_eq!(client.state, ClientState::Replaying);
        assert!(client.replay_pos > 0);
        assert!(client.outbound.len() > OUTBOUND_LIMIT - 2 * protocol::MAX_PAYLOAD);
    }

    /// After the queue drains, `continue_replay` must resume queueing so
    /// replay eventually completes. Without this, backpressure could turn
    /// into a deadlock.
    #[test]
    fn continue_replay_resumes_after_drain() {
        let (mut client, peer) = replaying_client(OUTBOUND_LIMIT * 4);
        let mut notified = false;

        // Fill outbound.
        for _ in 0..1_000 {
            assert!(continue_replay(&mut client, None, &mut notified));
        }
        let stalled_pos = client.replay_pos;
        assert!(client.outbound.len() > OUTBOUND_LIMIT - 2 * protocol::MAX_PAYLOAD);

        // Flush into the socketpair, then drain it on the peer side.
        assert!(client.flush());
        peer.set_nonblocking(true).unwrap();
        use std::io::Read;
        let mut sink = [0u8; 64 * 1024];
        let mut total = 0;
        loop {
            match (&peer).read(&mut sink) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        assert!(total > 0, "peer should have drained some bytes");

        // Now the server-side queue is empty (or nearly so); flush again
        // to push remaining outbound into the kernel buffer.
        assert!(client.flush());

        // continue_replay should now make progress past where we stalled.
        for _ in 0..100 {
            assert!(continue_replay(&mut client, None, &mut notified));
        }
        assert!(
            client.replay_pos > stalled_pos,
            "replay_pos did not advance after drain: still {stalled_pos}"
        );
    }

    /// `ReplayEnd` (and any trailing `Exit`) must also respect
    /// backpressure — they should not be silently skipped, but they also
    /// must not drop the client when the queue is full.
    #[test]
    fn continue_replay_end_is_gated_on_outbound_room() {
        // Tiny replay so we reach the "remaining == 0" branch quickly.
        let (mut client, _peer) = replaying_client(8);
        let mut notified = false;

        // Drain the small payload first.
        assert!(continue_replay(&mut client, None, &mut notified));

        // Now manually stuff outbound to nearly full so ReplayEnd won't fit.
        let pad = OUTBOUND_LIMIT - client.outbound.len();
        client.outbound.extend(std::iter::repeat_n(0u8, pad));
        assert_eq!(client.outbound.len(), OUTBOUND_LIMIT);

        // Should backpressure: stay alive, stay in Replaying.
        assert!(continue_replay(&mut client, None, &mut notified));
        assert_eq!(client.state, ClientState::Replaying);
        assert!(client.replay_buf.is_some());

        // Drop a few bytes to simulate flush progress.
        client.outbound.drain(..16);

        // Now ReplayEnd fits → state transitions to Live.
        assert!(continue_replay(&mut client, None, &mut notified));
        assert_eq!(client.state, ClientState::Live);
        assert!(client.replay_buf.is_none());
    }
}
