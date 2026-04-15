//! Multi-client tests.
//!
//! Verify controller election, readonly enforcement, ResizeReq handoff,
//! and that multiple clients receive the same PTY output.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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
    loop {
        match mneme::protocol::recv_packet(s.as_fd()) {
            Ok(pkt) => packets.push(pkt),
            Err(_) => break,
        }
    }
    packets
}

fn kill_server(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
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

    assert!(has_resize_req, "client A should receive ResizeReq after B detaches");

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
        pkts_b.iter().any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
        "B should get ResizeReq after C detaches"
    );
    // A should NOT get ResizeReq
    let pkts_a = drain_packets(&a, Duration::from_millis(300));
    assert!(
        !pkts_a.iter().any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
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
        pkts_a2.iter().any(|p| p.msg_type == mneme::protocol::MsgType::ResizeReq),
        "A should get ResizeReq after B detaches"
    );

    drop(a);
    kill_server(pid);
}
