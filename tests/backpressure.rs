//! Backpressure tests.
//!
//! Verify that a client that stops reading gets disconnected when the
//! server's per-client outbound buffer exceeds 256 KiB.

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

fn mn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

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

fn attach_raw(socket: &Path) -> UnixStream {
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

    // Drain Welcome + any Replay + ReplayEnd to reach Live state
    loop {
        let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
        if pkt.msg_type == mneme::protocol::MsgType::ReplayEnd {
            break;
        }
    }

    s
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
fn slow_client_gets_disconnected() {
    // Child produces a continuous flood of output (well over 256 KiB)
    let cmd = "while true; do dd if=/dev/zero bs=4096 count=1 2>/dev/null; done";
    let (dir, session, pid) = start_session("slow", cmd);

    let socket = dir.path().join(&session);

    // Attach a client and reach Live state
    let stream = attach_raw(&socket);

    // Now stop reading. The server will buffer output until it hits the
    // 256 KiB limit, then disconnect us.
    // We detect disconnection by attempting to read after a delay —
    // if the server closed us, read returns 0 or error.
    std::thread::sleep(Duration::from_secs(2));

    // Try to read — should get either data (if we're still connected)
    // or an error/EOF (if disconnected).
    let mut buf = [0u8; 4096];
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok();

    // Drain whatever is in the kernel socket buffer
    let mut total_read = 0;
    let mut got_error = false;
    for _ in 0..1000 {
        match (&stream).read(&mut buf) {
            Ok(0) => {
                got_error = true;
                break;
            }
            Ok(n) => total_read += n,
            Err(_) => {
                got_error = true;
                break;
            }
        }
    }

    // The server should have disconnected us. Either we got an error,
    // or we got some data but the connection is now dead.
    // If we read a lot of data without error, the server didn't apply
    // backpressure.
    assert!(
        got_error,
        "slow client was not disconnected — read {total_read} bytes without error"
    );

    // Server should still be alive (one slow client doesn't crash it)
    assert!(
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 },
        "server crashed"
    );

    drop(stream);
    kill_server(pid);
}

#[test]
fn fast_client_survives_while_slow_client_dropped() {
    // Child produces a flood
    let cmd = "while true; do dd if=/dev/zero bs=4096 count=1 2>/dev/null; done";
    let (dir, session, pid) = start_session("fast-vs-slow", cmd);

    let socket = dir.path().join(&session);

    // Attach a "slow" client that will never read
    let slow = attach_raw(&socket);

    // Attach a "fast" client that reads continuously
    let fast = attach_raw(&socket);

    // Let the slow client fall behind
    std::thread::sleep(Duration::from_secs(2));

    // The fast client should still be able to read
    fast.set_read_timeout(Some(Duration::from_secs(1))).ok();
    let mut buf = [0u8; 4096];
    let n = (&fast).read(&mut buf);
    assert!(
        matches!(n, Ok(n) if n > 0),
        "fast client should still receive data: {n:?}"
    );

    // Verify the slow client is disconnected
    slow.set_read_timeout(Some(Duration::from_secs(1))).ok();
    let mut total = 0;
    let mut disconnected = false;
    for _ in 0..1000 {
        match (&slow).read(&mut buf) {
            Ok(0) | Err(_) => {
                disconnected = true;
                break;
            }
            Ok(n) => total += n,
        }
    }
    assert!(
        disconnected,
        "slow client should be disconnected — read {total} bytes"
    );

    // Server alive
    assert!(
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 },
        "server crashed"
    );

    drop(fast);
    drop(slow);
    kill_server(pid);
}
