use crate::protocol::{self, ClientFlags, Intent, MsgType, Packet, PROTOCOL_VERSION};
use crate::ring::RingBuffer;

use rustix::event::{PollFd, PollFlags};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
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
    let cred = rustix::net::sockopt::get_socket_peercred(fd)?;
    let peer_uid = cred.uid;
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
    Connected,  // Hello not yet received
    Replaying,  // Sending replay data
    Live,       // Bidirectional
}

struct ServerClient {
    fd: OwnedFd,
    state: ClientState,
    flags: ClientFlags,
    outbound: VecDeque<u8>,
    replay_buf: Option<Vec<u8>>,  // snapshot being sent
    replay_pos: usize,            // position in replay_buf
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
                protocol::WriteResult::Error(_) => return false,
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
    session_name: String,
    ready_fd: i32,
    rows: u16,
    cols: u16,
    ring_size: usize,
    socket_path: PathBuf,
    command: Vec<String>,
}

fn parse_server_args(args: &[String]) -> Result<ServerOpts, String> {
    let mut ready_fd = None;
    let mut rows = 24u16;
    let mut cols = 80u16;
    let mut ring_size = 1024 * 1024usize;
    let mut socket_path = None;
    let mut command = Vec::new();

    if args.is_empty() {
        return Err("--server requires a session name".into());
    }
    let session_name = args[0].clone();
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
                        .ok_or("--ready-fd requires a value")?
                        .parse::<i32>()
                        .map_err(|e| format!("invalid --ready-fd: {e}"))?,
                );
            }
            "--rows" => {
                i += 1;
                rows = args
                    .get(i)
                    .ok_or("--rows requires a value")?
                    .parse()
                    .map_err(|e| format!("invalid --rows: {e}"))?;
            }
            "--cols" => {
                i += 1;
                cols = args
                    .get(i)
                    .ok_or("--cols requires a value")?
                    .parse()
                    .map_err(|e| format!("invalid --cols: {e}"))?;
            }
            "--ring-size" => {
                i += 1;
                ring_size = args
                    .get(i)
                    .ok_or("--ring-size requires a value")?
                    .parse()
                    .map_err(|e| format!("invalid --ring-size: {e}"))?;
            }
            "--socket-path" => {
                i += 1;
                socket_path = Some(PathBuf::from(
                    args.get(i).ok_or("--socket-path requires a value")?,
                ));
            }
            other => {
                return Err(format!("unknown server option: {other}"));
            }
        }
        i += 1;
    }

    Ok(ServerOpts {
        session_name,
        ready_fd: ready_fd.ok_or("--ready-fd is required")?,
        rows,
        cols,
        ring_size,
        socket_path: socket_path.ok_or("--socket-path is required")?,
        command,
    })
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

pub fn run_server(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_server_args(args).map_err(|e| e.to_string())?;

    // Take ownership of the ready fd and set CLOEXEC so the child doesn't inherit it
    let ready_fd = unsafe { OwnedFd::from_raw_fd(opts.ready_fd) };
    unsafe {
        libc::fcntl(opts.ready_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    // Fully detach from the calling session.
    // Note: setsid() may fail if we're already a process group leader
    // (which happens with process_group(0)). That's OK — the important
    // thing is that stdio is /dev/null and we're in a new process group.
    let _ = rustix::process::setsid();

    // Ignore SIGHUP as defense in depth
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // Bind + listen BEFORE spawning child — fail early
    let listener = bind_listen(&opts.socket_path)
        .map_err(|e| format!("bind {}: {e}", opts.socket_path.display()))?;
    listener.set_nonblocking(true)?;

    // Open PTY
    let (leader_fd, follower_fd) = open_pty()
        .map_err(|e| format!("openpty: {e}"))?;

    // Set initial terminal size on the PTY
    let winsize = libc::winsize {
        ws_row: opts.rows,
        ws_col: opts.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(leader_fd.as_raw_fd(), libc::TIOCSWINSZ, &winsize);
    }

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

fn spawn_child(
    command: &[String],
    follower_fd: OwnedFd,
) -> io::Result<std::process::Child> {
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
    signal_hook::low_level::pipe::register(libc::SIGCHLD, sig_write)?;

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
                handle_child_exit(&mut child, &mut child_running, &mut exit_status,
                                  &mut clients, &mut exit_notified);
            }
        }

        // Accept new clients
        if revents[0].contains(PollFlags::IN) {
            if let Ok((stream, _)) = listener.accept() {
                let fd = OwnedFd::from(stream);
                // Verify peer UID before accepting
                if verify_peer_uid(&fd).is_err() {
                    drop(fd); // reject connection
                } else {
                    rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).ok();
                    clients.push(ServerClient::new(fd));
                }
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
                    handle_child_exit(&mut child, &mut child_running, &mut exit_status,
                                      &mut clients, &mut exit_notified);
                }
                Err(e) => {
                    // EIO is common on macOS when child exits
                    if e.raw_os_error() == Some(libc::EIO) {
                        handle_child_exit(&mut child, &mut child_running, &mut exit_status,
                                          &mut clients, &mut exit_notified);
                    }
                    // else: ignore transient errors
                }
            }
        }

        // Process client I/O
        let mut to_remove: Vec<usize> = Vec::new();

        for ci in 0..clients.len() {
            let poll_idx = 3 + ci;
            if poll_idx >= revents.len() {
                break;
            }
            let rev = revents[poll_idx];
            let client = &mut clients[ci];

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
                    Ok(true) => {}  // continue
                    Ok(false) | Err(_) => {
                        to_remove.push(ci);
                        continue;
                    }
                }
            }

            // Continue replay if in progress
            if client.state == ClientState::Replaying {
                if !continue_replay(client, exit_status, &mut exit_notified) {
                    to_remove.push(ci);
                    continue;
                }
            }

            // Send PTY data to live clients
            if client.state == ClientState::Live {
                if let Some(ref data) = pty_data {
                    let pkt = Packet::content(data);
                    if !client.queue_packet(&pkt) {
                        to_remove.push(ci);
                        continue;
                    }
                }
            }

            // Flush outbound
            if rev.contains(PollFlags::OUT) || client.has_pending_output() {
                if !client.flush() {
                    to_remove.push(ci);
                    continue;
                }
            }
        }

        // Handle controller changes before removal
        let had_controller = clients.iter().any(|c| c.is_controller);
        remove_clients(&mut clients, &to_remove);
        if had_controller && !clients.iter().any(|c| c.is_controller) {
            elect_controller(&mut clients);
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
            // Expect a Hello
            let pkt = match protocol::recv_packet(client.fd.as_fd()) {
                Ok(p) => p,
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
                            client.is_controller = true;
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
                Err(_) => return Ok(false),
            };

            match pkt.msg_type {
                MsgType::Content => {
                    if !client.flags.contains(ClientFlags::READONLY) {
                        let _ = protocol::write_all_fd(leader_fd.as_fd(), &pkt.payload);
                    }
                }
                MsgType::Resize => {
                    if client.is_controller {
                        if let Some((rows, cols)) = pkt.parse_resize() {
                            let ws = libc::winsize {
                                ws_row: rows,
                                ws_col: cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            unsafe {
                                libc::ioctl(leader_fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
                                // Signal the child's process group to redraw.
                                // The child called setsid() so its PGID == its PID.
                                libc::kill(-(child_pid as libc::pid_t), libc::SIGWINCH);
                            }
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
            client.replay_buf = None;
            let end_pkt = Packet::empty(MsgType::ReplayEnd);
            if !client.queue_packet(&end_pkt) {
                return false;
            }
            client.state = ClientState::Live;

            if let Some(status) = exit_status {
                let exit_pkt = Packet::exit(status);
                if !client.queue_packet(&exit_pkt) {
                    return false;
                }
                *exit_notified = true;
            }

            if !client.flags.contains(ClientFlags::READONLY)
                && !client.flags.contains(ClientFlags::LOW_PRIORITY)
            {
                client.is_controller = true;
            }

            return true;
        }

        let chunk_size = remaining.min(protocol::MAX_PAYLOAD);
        let chunk = &buf[client.replay_pos..client.replay_pos + chunk_size];
        let pkt = Packet::replay(chunk);
        if !client.queue_packet(&pkt) {
            return false;
        }
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
        _ => Some(1),
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

fn elect_controller(clients: &mut [ServerClient]) {
    for c in clients.iter_mut().rev() {
        if c.state == ClientState::Live
            && !c.flags.contains(ClientFlags::READONLY)
            && !c.flags.contains(ClientFlags::LOW_PRIORITY)
        {
            c.is_controller = true;
            let pkt = Packet::empty(MsgType::ResizeReq);
            c.queue_packet(&pkt);
            return;
        }
    }
}

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
