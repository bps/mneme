//! PTY backpressure tests.
//!
//! With PTY-side backpressure (the server stops reading the PTY when
//! any live client's userspace outbound is full), slow clients are no
//! longer dropped. Instead the producer — the child process behind the
//! PTY — blocks on its writes, which throttles every reader at once.
//!
//! These tests verify the new semantics:
//!   * a slow client that stops reading is NOT disconnected,
//!   * a slow client also throttles fast clients in the same session,
//!   * once the slow client resumes, the session resumes too.

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn mn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

// Continuous producer: avoids fork-exec throttling that "while dd…" suffers
// from, so the producer rate is high enough to fill any reasonable buffer
// in a fraction of a second.
const FLOOD_CMD: &str = "exec cat /dev/zero";

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

fn server_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Non-draining peek: returns Some(true) if peer has hung up (recv would
/// return 0), Some(false) if peer is still connected (data buffered or
/// no data yet), None on weird errors.
fn peer_hung_up(stream: &UnixStream) -> Option<bool> {
    let mut buf = [0u8; 1];
    let r = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if r == 0 {
        return Some(true);
    }
    if r > 0 {
        return Some(false);
    }
    let err = std::io::Error::last_os_error().raw_os_error();
    match err {
        Some(libc::EAGAIN) | Some(libc::EINTR) => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn slow_client_is_not_disconnected() {
    // Slow client connects, never reads. With PTY backpressure the server
    // must NOT drop it; the child blocks on its writes instead.
    let (dir, session, pid) = start_session("slow-keep", FLOOD_CMD);
    let socket = dir.path().join(&session);

    let stream = attach_raw(&socket);

    // Wait long enough that, under the old "drop at 256 KiB outbound"
    // policy, the slow client would have been dropped many times over.
    std::thread::sleep(Duration::from_secs(3));

    match peer_hung_up(&stream) {
        Some(false) => {} // good — still connected
        Some(true) => panic!("server hung up on slow client (PTY backpressure not in effect)"),
        None => panic!("unexpected peer-state error"),
    }

    assert!(server_alive(pid), "server crashed");

    drop(stream);
    kill_server(pid);
}

#[test]
fn slow_client_throttles_fast_client() {
    // Two clients: one stops reading, one drains continuously. With PTY
    // backpressure the fast client must visibly slow down (the PTY
    // producer pauses while the slow client's outbound is full).
    let (dir, session, pid) = start_session("throttle", FLOOD_CMD);
    let socket = dir.path().join(&session);

    // Fast client first so its initial replay is small.
    let fast = attach_raw(&socket);
    let slow = attach_raw(&socket);

    // Baseline: drain fast for 500 ms with no slow-client interference yet
    // (slow's outbound hasn't filled). Measure throughput.
    fast.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut buf = [0u8; 8192];
    let baseline_start = Instant::now();
    let mut baseline_bytes: u64 = 0;
    while baseline_start.elapsed() < Duration::from_millis(500) {
        match (&fast).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => baseline_bytes += n as u64,
        }
    }

    // Now wait for slow's outbound to fill (gives time for kernel SNDBUF
    // + userspace buffer to saturate; once full, PTY backpressure kicks
    // in and fast is throttled too).
    std::thread::sleep(Duration::from_secs(1));

    // Measure fast's throughput while slow is stalled.
    let throttled_start = Instant::now();
    let mut throttled_bytes: u64 = 0;
    while throttled_start.elapsed() < Duration::from_millis(500) {
        match (&fast).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => throttled_bytes += n as u64,
        }
    }

    // The fast client should still be receiving SOMETHING (slow draining
    // through kernel buffer is still happening), but throughput should be
    // dramatically lower than baseline once PTY is paused.
    assert!(
        throttled_bytes < baseline_bytes / 4,
        "fast client wasn't throttled by slow client: baseline={baseline_bytes} \
         throttled={throttled_bytes}"
    );

    // Both clients must still be connected.
    assert_eq!(peer_hung_up(&fast), Some(false), "fast client got hung up");
    assert_eq!(peer_hung_up(&slow), Some(false), "slow client got hung up");
    assert!(server_alive(pid), "server crashed");

    drop(fast);
    drop(slow);
    kill_server(pid);
}

#[test]
fn session_resumes_when_slow_client_drains() {
    // Slow client stops reading → session pauses. Slow client resumes
    // reading → session resumes. Verify by measuring bytes read after
    // resume.
    let (dir, session, pid) = start_session("resume", FLOOD_CMD);
    let socket = dir.path().join(&session);

    let slow = attach_raw(&socket);
    std::thread::sleep(Duration::from_millis(800)); // let buffers fill

    // Now drain aggressively from slow; this should let the PTY resume.
    slow.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut buf = [0u8; 8192];
    let drain_start = Instant::now();
    let mut total: u64 = 0;
    while drain_start.elapsed() < Duration::from_secs(1) {
        match (&slow).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n as u64,
        }
    }

    // After 1s of aggressive draining from a /dev/zero source, we should
    // have read megabytes — far more than just the buffered backlog.
    assert!(
        total > 1024 * 1024,
        "session didn't resume after slow client drained: only {total} bytes in 1s"
    );

    assert!(server_alive(pid), "server crashed");
    drop(slow);
    kill_server(pid);
}
