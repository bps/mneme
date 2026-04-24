//! Backpressure tests.
//!
//! Verify that a client which stops reading gets disconnected once the
//! server's per-client outbound buffer exceeds OUTBOUND_LIMIT (256 KiB).
//!
//! These tests rely on the server capping the kernel SO_SNDBUF on
//! accepted clients to a small value (~64 KiB) so that the in-flight
//! data ceiling is bounded and observable across platforms.

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

/// Drain a connection (without producing more data) until we see EOF or
/// an error, with a hard byte cap that is comfortably above the worst-case
/// in-flight ceiling (kernel SNDBUF + OUTBOUND_LIMIT). Returns the number
/// of bytes drained and whether we observed disconnection.
fn drain_until_disconnect(stream: &UnixStream, max_bytes: usize) -> (usize, bool) {
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok();
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    while total < max_bytes {
        match (&*stream).read(&mut buf) {
            Ok(0) => return (total, true),
            Ok(n) => total += n,
            Err(_) => return (total, true),
        }
    }
    (total, false)
}

// Continuous producer. `exec` replaces the shell with cat so we don't
// pay fork-exec overhead per chunk; this fills the per-client outbound
// queue past OUTBOUND_LIMIT well within the test sleep window.
const FLOOD_CMD: &str = "exec cat /dev/zero";

// Cap on bytes we'll accept from a "supposedly disconnected" client
// before declaring backpressure broken. Has to exceed kernel SNDBUF
// (server caps to 64 KiB, kernel may double to 128 KiB) plus
// OUTBOUND_LIMIT (256 KiB), with a healthy margin.
const DISCONNECT_BYTE_CAP: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn slow_client_gets_disconnected() {
    let (dir, session, pid) = start_session("slow", FLOOD_CMD);
    let socket = dir.path().join(&session);

    // Attach a client and reach Live state, then stop reading. Server
    // should fill the kernel buffer, then the userspace outbound, then
    // disconnect us.
    let stream = attach_raw(&socket);
    std::thread::sleep(Duration::from_secs(2));

    let (total, disconnected) = drain_until_disconnect(&stream, DISCONNECT_BYTE_CAP);
    assert!(
        disconnected,
        "slow client was not disconnected — drained {total} bytes without EOF"
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
    let (dir, session, pid) = start_session("fast-vs-slow", FLOOD_CMD);
    let socket = dir.path().join(&session);

    // Slow client: never reads.
    let slow = attach_raw(&socket);

    // Fast client: drained continuously by a background thread so it
    // never falls behind.
    let fast = attach_raw(&socket);
    let fast_clone = fast.try_clone().expect("clone fast");
    let fast_reader = std::thread::spawn(move || -> u64 {
        let mut buf = [0u8; 8192];
        let mut total: u64 = 0;
        loop {
            match (&fast_clone).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n as u64,
                Err(_) => break,
            }
        }
        total
    });

    // Let the slow client fall behind.
    std::thread::sleep(Duration::from_secs(2));

    // Slow client must be disconnected within the byte cap.
    let (slow_total, disconnected) = drain_until_disconnect(&slow, DISCONNECT_BYTE_CAP);
    assert!(
        disconnected,
        "slow client was not disconnected — drained {slow_total} bytes without EOF"
    );

    // Server should still be alive.
    assert!(
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 },
        "server crashed"
    );

    // Tear down: closing the fast client's fd should cause the reader
    // thread to see EOF when the server eventually closes the peer
    // (or when we kill the server below).
    drop(fast);
    drop(slow);
    kill_server(pid);
    let fast_total = fast_reader.join().expect("reader thread");
    assert!(
        fast_total > 256 * 1024,
        "fast client received only {fast_total} bytes — was it backpressured too?"
    );
}
