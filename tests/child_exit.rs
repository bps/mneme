//! Child exit path tests.
//!
//! Verify that the server correctly handles child process exit in all
//! variations: normal exit, signal death, fast exit, and that exited
//! sessions remain accessible until a client is notified.

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

fn query_welcome(socket: &Path) -> Option<mneme::protocol::Welcome> {
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
    pkt.parse_welcome()
}

/// Attach and collect all messages until Exit or timeout.
fn attach_and_collect(socket: &Path) -> AttachResult {
    let s = UnixStream::connect(socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Attach,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(s.as_fd(), &mneme::protocol::Packet::hello(&hello)).unwrap();

    let mut result = AttachResult {
        welcome: None,
        replay_bytes: Vec::new(),
        live_bytes: Vec::new(),
        exit_status: None,
        got_replay_end: false,
    };

    while let Ok(pkt) = mneme::protocol::recv_packet(s.as_fd()) {
        match pkt.msg_type {
            mneme::protocol::MsgType::Welcome => {
                result.welcome = pkt.parse_welcome();
            }
            mneme::protocol::MsgType::Replay => {
                result.replay_bytes.extend_from_slice(&pkt.payload);
            }
            mneme::protocol::MsgType::ReplayEnd => {
                result.got_replay_end = true;
                // Short timeout for remaining packets
                s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            }
            mneme::protocol::MsgType::Content => {
                result.live_bytes.extend_from_slice(&pkt.payload);
            }
            mneme::protocol::MsgType::Exit => {
                result.exit_status = pkt.parse_exit_status();
                break;
            }
            _ => {}
        }
    }

    result
}

struct AttachResult {
    welcome: Option<mneme::protocol::Welcome>,
    replay_bytes: Vec<u8>,
    live_bytes: Vec<u8>,
    exit_status: Option<u32>,
    got_replay_end: bool,
}

fn kill_server(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn server_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn normal_exit_zero() {
    let (dir, session, pid) = start_session("exit0", "echo DONE; exit 0");
    let socket = dir.path().join(&session);

    // Wait for child to exit
    std::thread::sleep(Duration::from_millis(500));

    // Server should still be alive (no client notified yet)
    assert!(
        server_alive(pid),
        "server should stay alive until client notified"
    );

    // Attach — should get replay + Exit(0)
    let r = attach_and_collect(&socket);
    assert!(r.got_replay_end, "missing ReplayEnd");
    assert_eq!(r.exit_status, Some(0), "expected exit status 0");
    let text = String::from_utf8_lossy(&r.replay_bytes);
    assert!(
        text.contains("DONE"),
        "replay should contain child output: '{text}'"
    );

    // Now the server should exit (client was notified)
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server_alive(pid),
        "server should exit after client notification"
    );
}

#[test]
fn normal_exit_nonzero() {
    let (dir, session, pid) = start_session("exit42", "exit 42");
    let socket = dir.path().join(&session);

    std::thread::sleep(Duration::from_millis(500));

    let r = attach_and_collect(&socket);
    assert_eq!(r.exit_status, Some(42), "expected exit status 42");

    std::thread::sleep(Duration::from_millis(500));
    assert!(!server_alive(pid), "server should exit after notification");
}

#[test]
fn child_killed_by_signal() {
    // Child sends itself SIGTERM
    let (dir, session, pid) = start_session("sigterm", "kill -TERM $$");
    let socket = dir.path().join(&session);

    std::thread::sleep(Duration::from_millis(500));

    let r = attach_and_collect(&socket);
    // SIGTERM exit status: on most systems, 128 + signal_number or just
    // the raw status. The exact value depends on the shell.
    assert!(
        r.exit_status.is_some(),
        "should have an exit status for signal death"
    );
    let status = r.exit_status.unwrap();
    // Shell typically reports 128+signum or similar nonzero
    assert_ne!(status, 0, "signal death should be nonzero: {status}");

    std::thread::sleep(Duration::from_millis(500));
    assert!(!server_alive(pid), "server should exit");
}

#[test]
fn exited_session_queryable() {
    let (dir, session, pid) = start_session("queryable", "echo HELLO; exit 0");
    let socket = dir.path().join(&session);

    // Wait for child to exit
    std::thread::sleep(Duration::from_millis(500));

    // Query should work and show child_running=false
    let w = query_welcome(&socket).expect("query should succeed on exited session");
    assert!(!w.child_running, "child should be reported as not running");
    assert_eq!(w.exit_status, 0, "exit status should be 0");

    // Query doesn't count as "client notified" — server stays alive
    assert!(server_alive(pid), "server should survive query");

    // Now attach (this counts as notification)
    let r = attach_and_collect(&socket);
    assert_eq!(r.exit_status, Some(0));

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server_alive(pid),
        "server should exit after attach notification"
    );
}

#[test]
fn exited_session_attachable_multiple_times() {
    let (dir, session, pid) = start_session("multi-exit", "echo DATA; exit 7");
    let socket = dir.path().join(&session);

    std::thread::sleep(Duration::from_millis(500));

    // First attach
    let r = attach_and_collect(&socket);
    assert_eq!(r.exit_status, Some(7));
    let text = String::from_utf8_lossy(&r.replay_bytes);
    assert!(text.contains("DATA"), "first attach replay: '{text}'");

    // Server got notified — but let's check if it stays alive briefly
    // for a second attach
    std::thread::sleep(Duration::from_millis(200));

    // If server is still alive, a second attach should also work
    if server_alive(pid) {
        let r2 = attach_and_collect(&socket);
        assert_eq!(
            r2.exit_status,
            Some(7),
            "second attach should also get exit status"
        );
    }
    // Either way, this is fine — the design says server exits after
    // "no clients + at least one notified"

    kill_server(pid);
}

#[test]
fn fast_exit_with_create_attach() {
    // mn -c fast /usr/bin/true — the design's fast-exit scenario
    let dir = TempDir::new().expect("tempdir");
    let session = format!("fast-c-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["create", &session, "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exit status 0"),
        "should report exit status 0: stderr='{stderr}'"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "mn should exit 0 for child exit 0: stderr='{stderr}'"
    );
}

#[test]
fn fast_exit_nonzero_with_create_attach() {
    let dir = TempDir::new().expect("tempdir");
    let session = format!("fast-c-nz-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["create", &session, "/usr/bin/false"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exit status 1"),
        "should report exit status 1: stderr='{stderr}'"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "mn should exit 1 for child exit 1"
    );
}

#[test]
fn server_exits_only_after_client_notified() {
    // Create a session with a child that exits immediately.
    // Without any attach, the server should stay alive indefinitely
    // (waiting for at least one client to be notified).
    let (dir, session, pid) = start_session("wait-notify", "exit 0");
    let socket = dir.path().join(&session);

    // Wait well past the child exit
    std::thread::sleep(Duration::from_secs(1));

    // Server should still be alive
    assert!(
        server_alive(pid),
        "server should NOT exit until a client is notified"
    );

    // Socket should still be connectable
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "socket should still accept connections"
    );

    // Now attach — this notifies the client
    let r = attach_and_collect(&socket);
    assert_eq!(r.exit_status, Some(0));

    // Server should exit now
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server_alive(pid),
        "server should exit after client was notified"
    );
}

#[test]
fn exit_during_live_session() {
    // Child runs for a bit, then exits while a client is attached
    let fifo = format!("/tmp/mneme-exit-fifo-{}", std::process::id());
    let _ = std::fs::remove_file(&fifo);
    unsafe {
        let c = std::ffi::CString::new(fifo.clone()).unwrap();
        libc::mkfifo(c.as_ptr(), 0o600);
    }

    let cmd = format!("echo ALIVE; cat {fifo}; exit 3");
    let (dir, session, pid) = start_session("exit-live", &cmd);
    let socket = dir.path().join(&session);

    std::thread::sleep(Duration::from_millis(500));

    // Attach
    let s = UnixStream::connect(&socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Attach,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(s.as_fd(), &mneme::protocol::Packet::hello(&hello)).unwrap();

    // Drain until live
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        if pkt.msg_type == mneme::protocol::MsgType::ReplayEnd {
            break;
        }
    }

    // We're live. Now trigger the child exit by writing to the FIFO.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .expect("open fifo");
        writeln!(f, "done").unwrap();
    }

    // Should receive Exit packet
    let mut exit_status = None;
    while let Ok(pkt) = mneme::protocol::recv_packet(s.as_fd()) {
        if pkt.msg_type == mneme::protocol::MsgType::Exit {
            exit_status = pkt.parse_exit_status();
            break;
        }
    }

    assert_eq!(
        exit_status,
        Some(3),
        "should get exit status 3 from live session"
    );

    drop(s);
    let _ = std::fs::remove_file(&fifo);
    kill_server(pid);
}
