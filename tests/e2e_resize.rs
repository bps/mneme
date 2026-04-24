//! End-to-end resize tests using a PTY harness.
//!
//! These tests exercise the *full client-side path*: a real `mn attach`
//! process running inside a PTY we control. We resize the PTY via
//! `TIOCSWINSZ` on the leader fd and rely on kernel-generated SIGWINCH
//! (no manual signalling) to verify that:
//!
//! - The mn client catches SIGWINCH and sends a Resize packet
//! - The server applies the resize to the session PTY
//! - The child process (resize probe) observes the new dimensions
//! - Detach/reattach cycles preserve resize functionality
//! - Controller handoff + ResizeReq works end-to-end

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for waiting on markers.
const MARKER_TIMEOUT: Duration = Duration::from_secs(5);

/// Convert a Duration to a rustix Timespec for poll().
fn duration_to_timespec(d: Duration) -> Timespec {
    Timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    }
}

// ---------------------------------------------------------------------------
// Binary path
// ---------------------------------------------------------------------------

fn mn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

// ---------------------------------------------------------------------------
// PTY helpers
// ---------------------------------------------------------------------------

/// Open a PTY pair via libc::openpty. Sets CLOEXEC on the leader so it
/// is not inherited by child processes.
fn open_pty() -> (OwnedFd, OwnedFd) {
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
    assert!(
        ret == 0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    let leader = unsafe { OwnedFd::from_raw_fd(leader) };
    let follower = unsafe { OwnedFd::from_raw_fd(follower) };
    // Prevent leader from leaking into child processes
    rustix::io::fcntl_setfd(&leader, rustix::io::FdFlags::CLOEXEC).unwrap();
    (leader, follower)
}

/// Set the terminal dimensions on a PTY fd. When called on the leader,
/// the kernel automatically sends SIGWINCH to the foreground process group
/// of the slave side — no manual signalling needed.
fn set_winsize(fd: &OwnedFd, rows: u16, cols: u16) {
    let ws = rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(fd, ws).expect("tcsetwinsize failed");
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// Create a new session (server + child) using `mn new`.
/// Returns (TempDir, session_name, server_pid).
fn create_session(prefix: &str, shell_cmd: &str) -> (TempDir, String, u32) {
    let dir = TempDir::new().expect("tempdir");
    let session = format!("{prefix}-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["new", &session, "/bin/sh", "-c", shell_cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(
        output.status.success(),
        "mn new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    std::thread::sleep(Duration::from_millis(300));

    let socket = dir.path().join(&session);
    let pid = query_pid(&socket).expect("could not query server PID");
    (dir, session, pid)
}

fn query_pid(socket: &Path) -> Option<u32> {
    let s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Query,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 0,
        cols: 0,
    };
    mneme::protocol::send_packet(s.as_fd(), &mneme::protocol::Packet::hello(&hello)).ok()?;
    let pkt = mneme::protocol::recv_packet(s.as_fd()).ok()?;
    pkt.parse_welcome().map(|w| w.server_pid)
}

/// Return the shell command that runs the resize probe.
fn probe_cmd() -> String {
    let probe =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/resize_probe.py");
    format!("python3 {}", probe.display())
}

fn kill_server(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
}

// ---------------------------------------------------------------------------
// Spawning mn attach inside a PTY
// ---------------------------------------------------------------------------

/// Spawn `mn attach <session>` with the given PTY follower as its
/// controlling terminal.  The follower fd is consumed (closed in the
/// parent after spawn).
///
/// The child runs in a new session (`setsid`) with the PTY as its
/// controlling terminal (`TIOCSCTTY`), and is placed in the foreground
/// process group (`tcsetpgrp`).  This means `TIOCSWINSZ` on the leader
/// will produce a kernel-generated `SIGWINCH` to the client — exactly
/// as a real terminal emulator would.
fn spawn_mn_attach(socket_dir: &Path, session: &str, follower: OwnedFd) -> Child {
    use std::os::unix::process::CommandExt;

    let follower_file = std::fs::File::from(follower);
    let stdin_f = follower_file.try_clone().expect("clone follower for stdin");
    let stdout_f = follower_file
        .try_clone()
        .expect("clone follower for stdout");
    let stderr_f = follower_file; // consumes original

    let mut cmd = Command::new(mn_bin());
    cmd.env("MNEME_SOCKET_DIR", socket_dir)
        .args(["attach", session, "-q"])
        .stdin(Stdio::from(stdin_f))
        .stdout(Stdio::from(stdout_f))
        .stderr(Stdio::from(stderr_f));

    unsafe {
        cmd.pre_exec(|| {
            // New session — detach from any inherited controlling terminal
            libc::setsid();
            // Make the follower (fd 0) our controlling terminal
            libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
            // Ensure we are the foreground process group so that
            // TIOCSWINSZ on the leader generates SIGWINCH to us
            libc::tcsetpgrp(0, libc::getpid());
            Ok(())
        });
    }

    cmd.spawn().expect("failed to spawn mn attach")
}

// ---------------------------------------------------------------------------
// PTY output reader
// ---------------------------------------------------------------------------

/// Accumulated output reader for a PTY leader fd.
struct PtyReader {
    leader: OwnedFd,
    buf: Vec<u8>,
}

impl PtyReader {
    fn new(leader: OwnedFd) -> Self {
        Self {
            leader,
            buf: Vec::new(),
        }
    }

    /// Read from the leader until `marker` is found in the accumulated
    /// output, or `timeout` elapses.  Returns true if found.
    fn wait_for(&mut self, marker: &[u8], timeout: Duration) -> bool {
        if self.contains(marker) {
            return true;
        }

        let deadline = Instant::now() + timeout;
        let mut read_buf = [0u8; 4096];

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll_timeout = duration_to_timespec(remaining.min(Duration::from_millis(200)));

            let mut pollfds = [PollFd::new(&self.leader, PollFlags::IN)];
            match rustix::event::poll(&mut pollfds, Some(&poll_timeout)) {
                Ok(0) => continue,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
                Ok(_) => {}
            }

            if pollfds[0].revents().intersects(PollFlags::IN) {
                match rustix::io::read(&self.leader, &mut read_buf) {
                    Ok(n) if n > 0 => {
                        self.buf.extend_from_slice(&read_buf[..n]);
                        if self.contains(marker) {
                            return true;
                        }
                    }
                    Ok(_) => break,                      // EOF
                    Err(rustix::io::Errno::IO) => break, // EIO = PTY EOF
                    Err(rustix::io::Errno::AGAIN) => continue,
                    Err(_) => break,
                }
            }
            if pollfds[0]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR)
            {
                // Drain remaining bytes
                while let Ok(n) = rustix::io::read(&self.leader, &mut read_buf) {
                    if n == 0 {
                        break;
                    }
                    self.buf.extend_from_slice(&read_buf[..n]);
                }
                break;
            }
        }

        self.contains(marker)
    }

    /// Drain all immediately available output (with a short timeout).
    #[allow(dead_code)]
    fn drain(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut read_buf = [0u8; 4096];

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll_timeout = duration_to_timespec(remaining.min(Duration::from_millis(100)));

            let mut pollfds = [PollFd::new(&self.leader, PollFlags::IN)];
            match rustix::event::poll(&mut pollfds, Some(&poll_timeout)) {
                Ok(0) => break, // nothing more to read
                Err(_) => break,
                Ok(_) => {}
            }

            if pollfds[0].revents().intersects(PollFlags::IN) {
                match rustix::io::read(&self.leader, &mut read_buf) {
                    Ok(n) if n > 0 => self.buf.extend_from_slice(&read_buf[..n]),
                    _ => break,
                }
            } else {
                break;
            }
        }
    }

    /// Send bytes to the PTY (as if the user typed them).
    fn send(&self, data: &[u8]) {
        let _ = rustix::io::write(&self.leader, data);
    }

    /// Check whether the accumulated buffer contains a byte pattern.
    fn contains(&self, marker: &[u8]) -> bool {
        self.buf.windows(marker.len()).any(|w| w == marker)
    }

    /// Verify a marker is absent from the buffer.
    fn assert_absent(&self, marker: &[u8], msg: &str) {
        assert!(
            !self.contains(marker),
            "{msg}: found {:?} in output: {:?}",
            String::from_utf8_lossy(marker),
            self.recent_output()
        );
    }

    /// Return the last ~500 bytes as a lossy string for debug messages.
    fn recent_output(&self) -> String {
        let start = self.buf.len().saturating_sub(500);
        String::from_utf8_lossy(&self.buf[start..]).into_owned()
    }

    /// Clear the accumulated buffer (useful between test phases to avoid
    /// matching stale data).
    fn clear(&mut self) {
        self.buf.clear();
    }
}

// ---------------------------------------------------------------------------
// Raw socket client helper (for controller-handoff tests)
// ---------------------------------------------------------------------------

/// Attach a raw protocol client (not via PTY harness). Drains through
/// ReplayEnd to reach Live state.
fn attach_raw_client(socket: &Path, flags: mneme::protocol::ClientFlags) -> UnixStream {
    let s = UnixStream::connect(socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Attach,
        flags,
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(s.as_fd(), &mneme::protocol::Packet::hello(&hello)).unwrap();

    // Drain through Welcome + Replay* + ReplayEnd
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        if pkt.msg_type == mneme::protocol::MsgType::ReplayEnd {
            break;
        }
    }
    s
}

/// Read packets with a timeout, returning all received.
fn drain_packets(s: &UnixStream, timeout: Duration) -> Vec<mneme::protocol::Packet> {
    s.set_read_timeout(Some(timeout)).ok();
    let mut packets = Vec::new();
    while let Ok(pkt) = mneme::protocol::recv_packet(s.as_fd()) {
        packets.push(pkt);
    }
    packets
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic end-to-end resize: create session → attach via PTY → resize PTY →
/// verify the child sees the new dimensions.
#[test]
fn e2e_resize_basic() {
    let (dir, session, server_pid) = create_session("e2e-basic", &probe_cmd());

    let (leader, follower) = open_pty();
    set_winsize(&leader, 24, 80);
    let mut child = spawn_mn_attach(dir.path(), &session, follower);
    let mut pty = PtyReader::new(leader);

    // Wait for the probe's READY marker
    assert!(
        pty.wait_for(b"READY", MARKER_TIMEOUT),
        "probe READY not seen; output: {}",
        pty.recent_output()
    );

    // Resize the PTY — the kernel sends SIGWINCH to mn attach, which
    // sends Resize(40,100) to the server, which sets the session PTY
    // winsize and signals the probe.
    set_winsize(&pty.leader, 40, 100);

    assert!(
        pty.wait_for(b"WINCH 40 100", MARKER_TIMEOUT),
        "WINCH 40 100 not seen; output: {}",
        pty.recent_output()
    );

    let _ = child.kill();
    let _ = child.wait();
    kill_server(server_pid);
}

/// Multiple sequential resizes all propagate correctly.
#[test]
fn e2e_resize_multiple() {
    let (dir, session, server_pid) = create_session("e2e-multi", &probe_cmd());

    let (leader, follower) = open_pty();
    set_winsize(&leader, 24, 80);
    let mut child = spawn_mn_attach(dir.path(), &session, follower);
    let mut pty = PtyReader::new(leader);

    assert!(
        pty.wait_for(b"READY", MARKER_TIMEOUT),
        "probe READY not seen"
    );

    // Each resize uses unique dimensions
    let sizes: &[(u16, u16)] = &[(40, 100), (25, 132), (50, 200), (10, 40)];
    for &(rows, cols) in sizes {
        pty.clear(); // avoid matching stale output
        set_winsize(&pty.leader, rows, cols);

        let marker = format!("WINCH {rows} {cols}");
        assert!(
            pty.wait_for(marker.as_bytes(), MARKER_TIMEOUT),
            "{marker} not seen after resize; output: {}",
            pty.recent_output()
        );
    }

    let _ = child.kill();
    let _ = child.wait();
    kill_server(server_pid);
}

/// Detach, reattach, and verify resize still works in the new session.
#[test]
fn e2e_detach_reattach_resize() {
    let (dir, session, server_pid) = create_session("e2e-reattach", &probe_cmd());

    // --- First attachment ---
    let (leader1, follower1) = open_pty();
    set_winsize(&leader1, 24, 80);
    let mut child1 = spawn_mn_attach(dir.path(), &session, follower1);
    let mut pty1 = PtyReader::new(leader1);

    assert!(
        pty1.wait_for(b"READY", MARKER_TIMEOUT),
        "probe READY not seen on first attach"
    );

    // Resize to unique dimensions
    set_winsize(&pty1.leader, 35, 110);
    assert!(
        pty1.wait_for(b"WINCH 35 110", MARKER_TIMEOUT),
        "WINCH 35 110 not seen on first attach"
    );

    // Detach by sending the default detach key (Ctrl-\, byte 0x1C)
    pty1.send(b"\x1c");

    // Wait for mn attach to exit. Drain the PTY concurrently so the
    // client's final writes (soft reset, restore termios)
    // don't block on a full PTY buffer.
    let wait_start = Instant::now();
    loop {
        pty1.drain(Duration::from_millis(50));
        match child1.try_wait().expect("try_wait") {
            Some(s) => {
                assert!(
                    s.success(),
                    "mn attach exited with non-zero status on detach: {s}"
                );
                break;
            }
            None => {
                if wait_start.elapsed() > Duration::from_secs(3) {
                    let _ = child1.kill();
                    let _ = child1.wait();
                    panic!("mn attach did not exit within 3s after Ctrl-\\");
                }
            }
        }
    }

    // Small delay for the server to notice the detach
    std::thread::sleep(Duration::from_millis(300));

    // --- Second attachment ---
    let (leader2, follower2) = open_pty();
    set_winsize(&leader2, 24, 80);
    let mut child2 = spawn_mn_attach(dir.path(), &session, follower2);
    let mut pty2 = PtyReader::new(leader2);

    // The probe is still running; we should see READY in replay.
    // Wait for it plus any escape sequences to settle.
    assert!(
        pty2.wait_for(b"READY", MARKER_TIMEOUT),
        "probe READY not seen on reattach"
    );

    // Resize to different unique dimensions
    pty2.clear();
    set_winsize(&pty2.leader, 60, 160);
    assert!(
        pty2.wait_for(b"WINCH 60 160", MARKER_TIMEOUT),
        "WINCH 60 160 not seen after reattach resize; output: {}",
        pty2.recent_output()
    );

    let _ = child2.kill();
    let _ = child2.wait();
    kill_server(server_pid);
}

/// Controller handoff: when a second client takes control and then detaches,
/// the original client (via PTY harness) gets ResizeReq and re-announces
/// its terminal dimensions.
#[test]
fn e2e_resize_controller_handoff() {
    let (dir, session, server_pid) = create_session("e2e-handoff", &probe_cmd());
    let socket = dir.path().join(&session);

    // --- Client A: PTY harness (initial controller) ---
    let (leader, follower) = open_pty();
    set_winsize(&leader, 24, 80);
    let mut child_a = spawn_mn_attach(dir.path(), &session, follower);
    let mut pty = PtyReader::new(leader);

    assert!(
        pty.wait_for(b"READY", MARKER_TIMEOUT),
        "probe READY not seen"
    );

    // Prove A is controller: resize works
    set_winsize(&pty.leader, 35, 110);
    assert!(
        pty.wait_for(b"WINCH 35 110", MARKER_TIMEOUT),
        "WINCH 35 110 not seen (A should be controller)"
    );

    // --- Client B: raw socket (steals controller) ---
    let b = attach_raw_client(&socket, mneme::protocol::ClientFlags::empty());
    // Drain B's initial ResizeReq
    drain_packets(&b, Duration::from_millis(300));

    // Change A's PTY dimensions while A is NOT the controller.
    // A's mn client sends Resize(42,133), but the server ignores it.
    pty.clear();
    set_winsize(&pty.leader, 42, 133);

    // B (controller) sends a resize to prove it's in control
    let resize_b = mneme::protocol::Packet::resize(88, 188);
    mneme::protocol::send_packet(b.as_fd(), &resize_b).unwrap();

    // We should see WINCH 88 188 (B's resize), NOT WINCH 42 133 (A's was
    // ignored because A is not controller).
    assert!(
        pty.wait_for(b"WINCH 88 188", MARKER_TIMEOUT),
        "WINCH 88 188 not seen (B should be controller); output: {}",
        pty.recent_output()
    );
    pty.assert_absent(
        b"WINCH 42 133",
        "A's resize should have been ignored while A was not controller",
    );

    // --- Detach B → A becomes controller via ResizeReq ---
    pty.clear();
    mneme::protocol::send_packet(
        b.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(b);

    // When A gets ResizeReq, the mn client calls get_terminal_size() which
    // reads A's PTY (still 42×133). The server applies this and the probe
    // prints WINCH 42 133.
    assert!(
        pty.wait_for(b"WINCH 42 133", MARKER_TIMEOUT),
        "WINCH 42 133 not seen after controller handoff; output: {}",
        pty.recent_output()
    );

    let _ = child_a.kill();
    let _ = child_a.wait();
    kill_server(server_pid);
}

/// Server killed mid-attach: client should detect the hangup and exit
/// with status 1 (I/O error).  This drives the `DisconnectReason::ServerHungUp`
/// path in `client_mainloop` and the `IoError(..)` branch in `do_attach`.
#[test]
fn e2e_attach_exits_on_server_hangup() {
    let (dir, session, server_pid) =
        create_session("e2e-hangup", "/bin/sh -c 'echo READY; sleep 60'");

    let (leader, follower) = open_pty();
    set_winsize(&leader, 24, 80);
    let mut child = spawn_mn_attach(dir.path(), &session, follower);
    let mut pty = PtyReader::new(leader);

    // Wait until we know the client is fully attached & live.
    assert!(
        pty.wait_for(b"READY", MARKER_TIMEOUT),
        "READY not seen before hangup; output: {}",
        pty.recent_output()
    );

    // Yank the server out from under the attached client.
    kill_server(server_pid);

    // Drain PTY output during the wait so the client doesn't block on
    // stdout writes (e.g. the "I/O error" message + cursor restore).
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit_status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None => {
                // Drain a little to keep the PTY from backpressuring.
                pty.drain(Duration::from_millis(50));
                if Instant::now() >= deadline {
                    eprintln!("PTY tail: {}", pty.recent_output());
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("mn attach did not exit after server hangup");
                }
            }
        }
    };

    // do_attach returns Ok(1) for any IoError disconnect reason.
    assert_eq!(
        exit_status.code(),
        Some(1),
        "expected exit code 1 (I/O error) after server hangup, got {exit_status:?}"
    );
}
