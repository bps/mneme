//! Multi-client tests.
//!
//! Verify controller election, readonly enforcement, ResizeReq handoff,
//! resize propagation to PTY, and that multiple clients receive the same
//! PTY output.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn mn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn start_session(prefix: &str, shell_cmd: &str) -> (TempDir, String, u32) {
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
        "create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::thread::sleep(Duration::from_millis(300));

    let socket = dir.path().join(&session);
    let pid = query_pid(&socket).expect("query PID");
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

/// Attach a raw client with given flags. Drains through ReplayEnd to reach Live state.
fn attach_live(socket: &Path, flags: mneme::protocol::ClientFlags) -> UnixStream {
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

    // Drain Welcome + Replay* + ReplayEnd
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        if pkt.msg_type == mneme::protocol::MsgType::ReplayEnd {
            break;
        }
    }
    s
}

/// Read packets from a stream until timeout, returning all received.
fn drain_packets(s: &UnixStream, timeout: Duration) -> Vec<mneme::protocol::Packet> {
    s.set_read_timeout(Some(timeout)).ok();
    let mut packets = Vec::new();
    while let Ok(pkt) = mneme::protocol::recv_packet(s.as_fd()) {
        packets.push(pkt);
    }
    packets
}

fn kill_server(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
}

/// Return the shell command that runs the resize probe.
fn probe_cmd() -> String {
    let probe =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/resize_probe.py");
    format!("python3 {}", probe.display())
}

/// Read Content packets until `marker` appears in the accumulated text,
/// or until `timeout` elapses. Returns true if the marker was found.
/// Non-Content packets are silently skipped.
fn wait_for_content(s: &UnixStream, marker: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        s.set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
            .ok();
        match mneme::protocol::recv_packet(s.as_fd()) {
            Ok(pkt) if pkt.msg_type == mneme::protocol::MsgType::Content => {
                text.push_str(&String::from_utf8_lossy(&pkt.payload));
                if text.contains(marker) {
                    return true;
                }
            }
            Ok(_) => {} // skip ResizeReq, etc.
            Err(_) => {}
        }
    }
    false
}

/// Connect to a session, send Attach Hello, receive Welcome, but do NOT drain
/// through ReplayEnd. Returns the stream positioned just after Welcome.
fn attach_before_replay(socket: &Path, flags: mneme::protocol::ClientFlags) -> UnixStream {
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

    // Receive Welcome only
    let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
    assert_eq!(pkt.msg_type, mneme::protocol::MsgType::Welcome);
    s
}

/// Attach to a session, drain through ReplayEnd, and return both the stream
/// (in Live state) and the replay text. Useful when the test needs to verify
/// that content appeared in replay.
fn attach_live_with_replay(
    socket: &Path,
    flags: mneme::protocol::ClientFlags,
) -> (UnixStream, String) {
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

    let mut replay_text = String::new();
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        match pkt.msg_type {
            mneme::protocol::MsgType::Welcome => {}
            mneme::protocol::MsgType::Replay => {
                replay_text.push_str(&String::from_utf8_lossy(&pkt.payload));
            }
            mneme::protocol::MsgType::ReplayEnd => break,
            other => panic!("unexpected packet during attach: {other:?}"),
        }
    }
    (s, replay_text)
}

/// Check for a marker in replay text or subsequent Content packets.
fn has_marker(s: &UnixStream, replay_text: &str, marker: &str, timeout: Duration) -> bool {
    if replay_text.contains(marker) {
        return true;
    }
    wait_for_content(s, marker, timeout)
}

/// Drain through ReplayEnd, returning all Replay bytes. Panics if ReplayEnd
/// is not received.
#[allow(dead_code)]
fn drain_replay(s: &UnixStream) {
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        match pkt.msg_type {
            mneme::protocol::MsgType::ReplayEnd => return,
            mneme::protocol::MsgType::Replay => {}
            other => panic!("unexpected packet during replay drain: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn both_clients_receive_pty_output() {
    // Use a FIFO to control when output is produced
    let fifo = format!("/tmp/mneme-mc-fifo-{}", std::process::id());
    let _ = std::fs::remove_file(&fifo);
    unsafe {
        let c = std::ffi::CString::new(fifo.clone()).unwrap();
        libc::mkfifo(c.as_ptr(), 0o600);
    }

    let cmd = format!("cat {fifo}; sleep 60");
    let (dir, session, pid) = start_session("both-recv", &cmd);
    let socket = dir.path().join(&session);

    let flags = mneme::protocol::ClientFlags::empty();
    let c1 = attach_live(&socket, flags);
    let c2 = attach_live(&socket, flags);

    // Write to the FIFO — both clients should see it
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .expect("open fifo");
        writeln!(f, "SHARED_OUTPUT").unwrap();
    }

    std::thread::sleep(Duration::from_millis(500));

    // Read from both clients
    let pkts1 = drain_packets(&c1, Duration::from_millis(500));
    let pkts2 = drain_packets(&c2, Duration::from_millis(500));

    let text1: String = pkts1
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();
    let text2: String = pkts2
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();

    assert!(
        text1.contains("SHARED_OUTPUT"),
        "client 1 missing output: '{text1}'"
    );
    assert!(
        text2.contains("SHARED_OUTPUT"),
        "client 2 missing output: '{text2}'"
    );

    drop(c1);
    drop(c2);
    let _ = std::fs::remove_file(&fifo);
    kill_server(pid);
}

#[test]
fn readonly_client_input_ignored() {
    // Child uses cat — echoes stdin back to stdout.
    // A readonly client's Content should be silently dropped.
    let fifo = format!("/tmp/mneme-ro-fifo-{}", std::process::id());
    let _ = std::fs::remove_file(&fifo);
    unsafe {
        let c = std::ffi::CString::new(fifo.clone()).unwrap();
        libc::mkfifo(c.as_ptr(), 0o600);
    }

    let cmd = format!("cat {fifo}; sleep 60");
    let (dir, session, pid) = start_session("readonly", &cmd);
    let socket = dir.path().join(&session);

    // Attach a readonly client
    let ro = attach_live(&socket, mneme::protocol::ClientFlags::READONLY);

    // Send Content from the readonly client — it should be ignored
    let pkt = mneme::protocol::Packet::content(b"SHOULD_NOT_ECHO\n");
    mneme::protocol::send_packet(ro.as_fd(), &pkt).unwrap();

    // Also attach a normal client to observe
    let normal = attach_live(&socket, mneme::protocol::ClientFlags::empty());

    // Write to FIFO to produce known output
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .expect("open fifo");
        writeln!(f, "REAL_OUTPUT").unwrap();
    }

    std::thread::sleep(Duration::from_millis(500));

    // Read from the normal client — should see REAL_OUTPUT but NOT SHOULD_NOT_ECHO
    let pkts = drain_packets(&normal, Duration::from_millis(500));
    let text: String = pkts
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();

    assert!(
        text.contains("REAL_OUTPUT"),
        "normal client missing output: '{text}'"
    );
    assert!(
        !text.contains("SHOULD_NOT_ECHO"),
        "readonly client's input was echoed: '{text}'"
    );

    drop(ro);
    drop(normal);
    let _ = std::fs::remove_file(&fifo);
    kill_server(pid);
}

#[test]
fn controller_detach_sends_resize_req() {
    let (dir, session, pid) = start_session("resize-req", "sleep 60");
    let socket = dir.path().join(&session);

    // Attach client A (becomes controller)
    let a = attach_live(&socket, mneme::protocol::ClientFlags::empty());
    // Attach client B (not controller — A is more recent? Actually B is newer)
    // B becomes controller since it's the most recently attached.
    let b = attach_live(&socket, mneme::protocol::ClientFlags::empty());

    // Detach B (the controller) — server should promote A and send ResizeReq to A
    mneme::protocol::send_packet(
        b.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(b);

    std::thread::sleep(Duration::from_millis(300));

    // Read from A — should get a ResizeReq
    let pkts = drain_packets(&a, Duration::from_millis(500));
    let has_resize_req = pkts
        .iter()
        .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq);

    assert!(
        has_resize_req,
        "client A should receive ResizeReq after B detaches"
    );

    drop(a);
    kill_server(pid);
}

#[test]
fn readonly_client_does_not_become_controller() {
    let (dir, session, pid) = start_session("ro-no-ctrl", "sleep 60");
    let socket = dir.path().join(&session);

    // Attach normal client (controller)
    let normal = attach_live(&socket, mneme::protocol::ClientFlags::empty());
    // Attach readonly client
    let ro = attach_live(&socket, mneme::protocol::ClientFlags::READONLY);

    // Detach normal (the controller). The readonly client should NOT
    // become controller and should NOT get ResizeReq.
    mneme::protocol::send_packet(
        normal.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(normal);

    std::thread::sleep(Duration::from_millis(300));

    let pkts = drain_packets(&ro, Duration::from_millis(500));
    let has_resize_req = pkts
        .iter()
        .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq);

    assert!(
        !has_resize_req,
        "readonly client should NOT receive ResizeReq"
    );

    drop(ro);
    kill_server(pid);
}

#[test]
fn low_priority_client_does_not_become_controller() {
    let (dir, session, pid) = start_session("lp-no-ctrl", "sleep 60");
    let socket = dir.path().join(&session);

    let normal = attach_live(&socket, mneme::protocol::ClientFlags::empty());
    let lp = attach_live(&socket, mneme::protocol::ClientFlags::LOW_PRIORITY);

    // Detach normal
    mneme::protocol::send_packet(
        normal.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(normal);

    std::thread::sleep(Duration::from_millis(300));

    let pkts = drain_packets(&lp, Duration::from_millis(500));
    let has_resize_req = pkts
        .iter()
        .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq);

    assert!(
        !has_resize_req,
        "low-priority client should NOT receive ResizeReq"
    );

    drop(lp);
    kill_server(pid);
}

#[test]
fn three_clients_controller_handoff_chain() {
    let (dir, session, pid) = start_session("chain", "sleep 60");
    let socket = dir.path().join(&session);

    let flags = mneme::protocol::ClientFlags::empty();

    // Attach A, B, C in order — each triggers re-election
    let a = attach_live(&socket, flags);
    // Drain A's initial ResizeReq (A was controller briefly)
    drain_packets(&a, Duration::from_millis(300));

    let b = attach_live(&socket, flags);
    // Drain B's ResizeReq (B became controller when it attached)
    drain_packets(&b, Duration::from_millis(300));
    // A may have gotten a demotion — drain it too
    drain_packets(&a, Duration::from_millis(100));

    let c = attach_live(&socket, flags);
    // Drain C's ResizeReq
    drain_packets(&c, Duration::from_millis(300));
    drain_packets(&b, Duration::from_millis(100));
    drain_packets(&a, Duration::from_millis(100));

    // Detach C → B becomes controller, gets ResizeReq
    mneme::protocol::send_packet(
        c.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(c);
    std::thread::sleep(Duration::from_millis(300));

    let pkts_b = drain_packets(&b, Duration::from_millis(500));
    assert!(
        pkts_b
            .iter()
            .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
        "B should get ResizeReq after C detaches"
    );
    // A should NOT get ResizeReq
    let pkts_a = drain_packets(&a, Duration::from_millis(300));
    assert!(
        !pkts_a
            .iter()
            .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
        "A should NOT get ResizeReq (B is controller)"
    );

    // Detach B → A becomes controller, gets ResizeReq
    mneme::protocol::send_packet(
        b.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();
    drop(b);
    std::thread::sleep(Duration::from_millis(300));

    let pkts_a2 = drain_packets(&a, Duration::from_millis(500));
    assert!(
        pkts_a2
            .iter()
            .any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
        "A should get ResizeReq after B detaches"
    );

    drop(a);
    kill_server(pid);
}

// ---------------------------------------------------------------------------
// Resize integration tests
// ---------------------------------------------------------------------------

/// Verify that a Resize from the controlling client actually changes the PTY
/// dimensions (the server calls tcsetwinsize + SIGWINCH on the child).
#[test]
fn controller_resize_changes_pty() {
    let (dir, session, pid) = start_session("resize-pty", &probe_cmd());
    let socket = dir.path().join(&session);

    let (a, replay) = attach_live_with_replay(&socket, mneme::protocol::ClientFlags::empty());

    // Verify the probe started (READY may be in replay or in subsequent Content)
    assert!(
        has_marker(&a, &replay, "READY", Duration::from_secs(3)),
        "probe did not print READY"
    );

    // Drain any ResizeReq from becoming controller
    drain_packets(&a, Duration::from_millis(200));

    // Send Resize(40, 100) — we are the controller
    let resize = mneme::protocol::Packet::resize(40, 100);
    mneme::protocol::send_packet(a.as_fd(), &resize).unwrap();

    // The server sets the PTY winsize and sends SIGWINCH to the child.
    // The probe catches SIGWINCH, queries TIOCGWINSZ, and prints "WINCH 40 100".
    assert!(
        wait_for_content(&a, "WINCH 40 100", Duration::from_secs(3)),
        "probe did not report WINCH 40 100 after controller resize"
    );

    // Send a second resize to verify repeated resizes work
    let resize2 = mneme::protocol::Packet::resize(25, 132);
    mneme::protocol::send_packet(a.as_fd(), &resize2).unwrap();

    assert!(
        wait_for_content(&a, "WINCH 25 132", Duration::from_secs(3)),
        "probe did not report WINCH 25 132 after second resize"
    );

    drop(a);
    kill_server(pid);
}

/// Verify that a Resize from a non-controlling client is silently ignored
/// (the PTY dimensions do not change and no SIGWINCH is sent).
#[test]
fn non_controller_resize_ignored() {
    let (dir, session, pid) = start_session("resize-noctl", &probe_cmd());
    let socket = dir.path().join(&session);

    // A attaches first (becomes controller)
    let (a, replay) = attach_live_with_replay(&socket, mneme::protocol::ClientFlags::empty());
    assert!(
        has_marker(&a, &replay, "READY", Duration::from_secs(3)),
        "probe did not print READY"
    );

    // B attaches second — B becomes the new controller (most recent)
    let b = attach_live(&socket, mneme::protocol::ClientFlags::empty());

    // Drain ResizeReq and other setup packets from both
    drain_packets(&a, Duration::from_millis(300));
    drain_packets(&b, Duration::from_millis(300));

    // A (non-controller) sends Resize(99, 77) — should be silently ignored
    let resize_a = mneme::protocol::Packet::resize(99, 77);
    mneme::protocol::send_packet(a.as_fd(), &resize_a).unwrap();

    // Wait to give the server time to (not) process it
    std::thread::sleep(Duration::from_millis(500));

    // Check that NO "WINCH 99 77" arrived at either client
    let pkts_a = drain_packets(&a, Duration::from_millis(300));
    let pkts_b = drain_packets(&b, Duration::from_millis(300));
    let text_a: String = pkts_a
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();
    let text_b: String = pkts_b
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();
    assert!(
        !text_a.contains("WINCH 99 77"),
        "non-controller resize should be ignored, but A saw: '{text_a}'"
    );
    assert!(
        !text_b.contains("WINCH 99 77"),
        "non-controller resize should be ignored, but B saw: '{text_b}'"
    );

    // Now B (controller) sends Resize(50, 120) — should be applied
    let resize_b = mneme::protocol::Packet::resize(50, 120);
    mneme::protocol::send_packet(b.as_fd(), &resize_b).unwrap();

    // Both clients should see the WINCH output (they both get Content from the PTY)
    assert!(
        wait_for_content(&a, "WINCH 50 120", Duration::from_secs(3))
            || wait_for_content(&b, "WINCH 50 120", Duration::from_secs(3)),
        "controller resize should have produced WINCH 50 120"
    );

    drop(a);
    drop(b);
    kill_server(pid);
}

/// Verify that a Resize sent while the client is still in Replaying state
/// is ignored by the server, and that the connection remains synchronized
/// afterward (the client can communicate normally once live).
#[test]
fn resize_during_replay_ignored() {
    // Start a session that produces enough output to have replay data,
    // then runs the probe for post-replay verification.
    let probe =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/resize_probe.py");
    let cmd = format!(
        "dd if=/dev/urandom bs=1024 count=10 2>/dev/null | base64; exec python3 {}",
        probe.display()
    );
    let (dir, session, pid) = start_session("replay-resize", &cmd);
    let socket = dir.path().join(&session);

    // Wait for the output to be produced and stored in the ring buffer
    std::thread::sleep(Duration::from_millis(800));

    // Attach but stop after Welcome — we are now in Replaying state
    let s = attach_before_replay(&socket, mneme::protocol::ClientFlags::empty());

    // Read one Replay packet to confirm we are indeed in replay
    let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
    assert_eq!(
        pkt.msg_type,
        mneme::protocol::MsgType::Replay,
        "expected Replay packet, got {:?}",
        pkt.msg_type
    );

    // Send Resize while server still considers us in Replaying state
    let resize = mneme::protocol::Packet::resize(55, 200);
    mneme::protocol::send_packet(s.as_fd(), &resize).unwrap();

    // Drain through ReplayEnd — the connection should still be synchronized.
    // Collect replay text to check for READY (it may appear in replay data).
    let mut replay_text = String::new();
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        match pkt.msg_type {
            mneme::protocol::MsgType::ReplayEnd => break,
            mneme::protocol::MsgType::Replay => {
                replay_text.push_str(&String::from_utf8_lossy(&pkt.payload));
            }
            other => panic!("unexpected packet during replay: {other:?}"),
        }
    }

    // We are now Live. Wait for the probe to be ready.
    assert!(
        has_marker(&s, &replay_text, "READY", Duration::from_secs(5)),
        "probe did not print READY after replay"
    );

    // Drain any ResizeReq from becoming controller
    drain_packets(&s, Duration::from_millis(200));

    // Verify the connection is still functional by sending a resize that
    // should work now that we're the controller.
    let resize2 = mneme::protocol::Packet::resize(30, 90);
    mneme::protocol::send_packet(s.as_fd(), &resize2).unwrap();

    assert!(
        wait_for_content(&s, "WINCH 30 90", Duration::from_secs(3)),
        "post-replay resize should work (WINCH 30 90 not seen)"
    );

    drop(s);
    kill_server(pid);
}

/// Verify that a Resize from a readonly client is silently ignored.
#[test]
fn readonly_resize_ignored() {
    let (dir, session, pid) = start_session("ro-resize", &probe_cmd());
    let socket = dir.path().join(&session);

    // Attach a normal client (becomes controller)
    let (normal, replay) = attach_live_with_replay(&socket, mneme::protocol::ClientFlags::empty());
    assert!(
        has_marker(&normal, &replay, "READY", Duration::from_secs(3)),
        "probe did not print READY"
    );

    // Attach a readonly client
    let ro = attach_live(&socket, mneme::protocol::ClientFlags::READONLY);
    drain_packets(&normal, Duration::from_millis(300));
    drain_packets(&ro, Duration::from_millis(300));

    // Readonly client sends Resize — should be silently ignored
    let resize = mneme::protocol::Packet::resize(88, 44);
    mneme::protocol::send_packet(ro.as_fd(), &resize).unwrap();

    std::thread::sleep(Duration::from_millis(500));

    let pkts = drain_packets(&normal, Duration::from_millis(300));
    let text: String = pkts
        .iter()
        .filter(|p| p.msg_type == mneme::protocol::MsgType::Content)
        .flat_map(|p| p.payload.iter().copied())
        .map(|b| b as char)
        .collect();
    assert!(
        !text.contains("WINCH 88 44"),
        "readonly resize should be ignored, but saw: '{text}'"
    );

    drop(ro);
    drop(normal);
    kill_server(pid);
}
