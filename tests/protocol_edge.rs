//! Protocol edge case tests.
//!
//! These tests connect raw Unix sockets to a running server and send
//! malformed or unexpected packets to verify the server handles them
//! gracefully (disconnects, sends Error, doesn't crash).

use std::io::{Read, Write};
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
// Test harness (minimal — just needs a running server)
// ---------------------------------------------------------------------------

struct ServerFixture {
    dir: TempDir,
    session: String,
    server_pid: u32,
}

impl ServerFixture {
    /// Start a server with a long-running child, return a fixture for testing.
    fn start(prefix: &str) -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let session = format!("{prefix}-{}", std::process::id());

        let mut cmd = Command::new(mn_bin());
        cmd.env("MNEME_SOCKET_DIR", dir.path())
            .args(["new", &session, "/bin/sh", "-c", "sleep 300"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let child = cmd.spawn().expect("failed to spawn mn");
        let output = child.wait_with_output().expect("failed to wait");
        assert!(
            output.status.success(),
            "create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Wait for server to be ready
        std::thread::sleep(Duration::from_millis(300));

        let mut fix = Self {
            dir,
            session,
            server_pid: 0,
        };

        // Get server PID
        let sp = fix.socket_path();
        let pid = query_pid(&sp).expect("failed to query server PID");
        fix.server_pid = pid;
        fix
    }

    fn socket_path(&self) -> std::path::PathBuf {
        self.dir.path().join(&self.session)
    }

    /// Connect a raw Unix socket to the server.
    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(self.socket_path()).expect("connect failed");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .ok();
        stream
    }

    /// Check that the server is still alive.
    fn server_alive(&self) -> bool {
        unsafe { libc::kill(self.server_pid as libc::pid_t, 0) == 0 }
    }
}

impl Drop for ServerFixture {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.server_pid as libc::pid_t, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn query_pid(socket_path: &Path) -> Option<u32> {
    let stream = UnixStream::connect(socket_path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .ok()?;
    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Query,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 0,
        cols: 0,
    };
    mneme::protocol::send_packet(
        stream.as_fd(),
        &mneme::protocol::Packet::hello(&hello),
    )
    .ok()?;
    let pkt = mneme::protocol::recv_packet(stream.as_fd()).ok()?;
    pkt.parse_welcome().map(|w| w.server_pid)
}

// ---------------------------------------------------------------------------
// Raw protocol helpers
// ---------------------------------------------------------------------------

/// Send raw bytes on a stream.
fn send_raw(stream: &mut UnixStream, bytes: &[u8]) {
    stream.write_all(bytes).expect("raw write failed");
}

/// Build a raw packet: [type:u8][len:u16 LE][payload]
fn raw_packet(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut buf = Vec::with_capacity(3 + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Try to read a response. Returns the bytes read (may be empty on timeout).
fn read_response(stream: &mut UnixStream) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Try to receive a typed packet from the stream.
fn recv_typed(stream: &UnixStream) -> Option<mneme::protocol::Packet> {
    mneme::protocol::recv_packet(stream.as_fd()).ok()
}

// ---------------------------------------------------------------------------
// Protocol unit tests (no server needed)
// ---------------------------------------------------------------------------

#[test]
fn unknown_message_type_rejected() {
    // MsgType::from_u8 should return None for invalid types
    assert!(mneme::protocol::MsgType::from_u8(255).is_none());
    assert!(mneme::protocol::MsgType::from_u8(10).is_none());
    assert!(mneme::protocol::MsgType::from_u8(99).is_none());
}

#[test]
fn invalid_intent_rejected() {
    assert!(mneme::protocol::Intent::from_u8(2).is_none());
    assert!(mneme::protocol::Intent::from_u8(255).is_none());
}

#[test]
fn parse_hello_too_short() {
    let pkt = mneme::protocol::Packet::new(mneme::protocol::MsgType::Hello, vec![1, 2, 3]);
    assert!(pkt.parse_hello().is_none(), "should reject short payload");
}

#[test]
fn parse_welcome_too_short() {
    let pkt = mneme::protocol::Packet::new(mneme::protocol::MsgType::Welcome, vec![1; 10]);
    assert!(pkt.parse_welcome().is_none(), "should reject short payload");
}

#[test]
fn parse_resize_too_short() {
    let pkt = mneme::protocol::Packet::new(mneme::protocol::MsgType::Resize, vec![1, 2]);
    assert!(pkt.parse_resize().is_none(), "should reject short payload");
}

#[test]
fn parse_exit_too_short() {
    let pkt = mneme::protocol::Packet::new(mneme::protocol::MsgType::Exit, vec![1]);
    assert!(pkt.parse_exit_status().is_none(), "should reject short payload");
}

#[test]
fn parse_wrong_type_returns_none() {
    let pkt = mneme::protocol::Packet::content(b"hello");
    assert!(pkt.parse_hello().is_none());
    assert!(pkt.parse_welcome().is_none());
    assert!(pkt.parse_resize().is_none());
    assert!(pkt.parse_exit_status().is_none());
    assert!(pkt.parse_error().is_none());
}

#[test]
fn recv_packet_rejects_unknown_type() {
    use std::os::fd::AsFd;
    let (r, w) = rustix::pipe::pipe().unwrap();
    let r = std::fs::File::from(r);
    let w = std::fs::File::from(w);

    // Send a packet with type=99 (invalid)
    let bad = raw_packet(99, b"hello");
    (&w).write_all(&bad).unwrap();
    drop(w);

    let result = mneme::protocol::recv_packet(r.as_fd());
    assert!(result.is_err(), "should reject unknown type");
}

#[test]
fn recv_packet_rejects_oversized_payload() {
    use std::os::fd::AsFd;
    let (r, w) = rustix::pipe::pipe().unwrap();
    let r = std::fs::File::from(r);
    let w = std::fs::File::from(w);

    // Craft a header claiming 65535 bytes of payload
    let mut header = vec![3u8]; // Content type
    header.extend_from_slice(&65535u16.to_le_bytes());
    (&w).write_all(&header).unwrap();
    drop(w);

    let result = mneme::protocol::recv_packet(r.as_fd());
    assert!(result.is_err(), "should reject oversized payload");
}

// ---------------------------------------------------------------------------
// Server-level edge case tests
// ---------------------------------------------------------------------------

#[test]
fn server_rejects_wrong_version() {
    let fix = ServerFixture::start("ver-mismatch");
    let stream = fix.connect();

    // Send Hello with version 99
    let hello = mneme::protocol::Hello {
        version: 99,
        intent: mneme::protocol::Intent::Attach,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(stream.as_fd(), &mneme::protocol::Packet::hello(&hello))
        .unwrap();

    // Should get an Error response
    let pkt = recv_typed(&stream);
    match pkt {
        Some(p) if p.msg_type == mneme::protocol::MsgType::Error => {
            let msg = p.parse_error().unwrap_or_default();
            assert!(
                msg.contains("version mismatch"),
                "wrong error: {msg}"
            );
        }
        Some(p) => panic!("expected Error, got {:?}", p.msg_type),
        None => {} // server disconnected, also acceptable
    }

    // Server should still be alive (one bad client doesn't crash it)
    assert!(fix.server_alive(), "server crashed");
}

#[test]
fn server_rejects_garbage_first_message() {
    let fix = ServerFixture::start("garbage");
    let stream = fix.connect();

    // Send a complete packet with an invalid type (type=99)
    let pkt = raw_packet(99, b"JUNK");
    (&stream).write_all(&pkt).expect("write failed");

    // Server should disconnect us
    std::thread::sleep(Duration::from_millis(300));

    // Server still alive
    assert!(fix.server_alive(), "server crashed on garbage input");
}

#[test]
fn server_rejects_content_before_hello() {
    let fix = ServerFixture::start("no-hello");
    let stream = fix.connect();

    // Send Content without Hello first
    let pkt = mneme::protocol::Packet::content(b"sneaky data");
    mneme::protocol::send_packet(stream.as_fd(), &pkt).unwrap();

    // Server should send Error or disconnect
    std::thread::sleep(Duration::from_millis(200));
    let resp = recv_typed(&stream);
    match resp {
        Some(p) if p.msg_type == mneme::protocol::MsgType::Error => {
            let msg = p.parse_error().unwrap_or_default();
            assert!(msg.contains("expected Hello"), "wrong error: {msg}");
        }
        Some(p) => panic!("expected Error, got {:?}", p.msg_type),
        None => {} // disconnected, acceptable
    }

    assert!(fix.server_alive(), "server crashed");
}

#[test]
fn server_handles_truncated_connection() {
    let fix = ServerFixture::start("truncated");
    let stream = fix.connect();

    // Connect and immediately close without sending a complete packet
    drop(stream);

    std::thread::sleep(Duration::from_millis(300));
    assert!(fix.server_alive(), "server crashed on truncated connection");
}

#[test]
fn server_handles_immediate_disconnect() {
    let fix = ServerFixture::start("imm-disconnect");
    let stream = fix.connect();

    // Connect and immediately close without sending anything
    drop(stream);

    std::thread::sleep(Duration::from_millis(200));
    assert!(fix.server_alive(), "server crashed on immediate disconnect");
}

#[test]
fn server_survives_multiple_bad_clients() {
    let fix = ServerFixture::start("multi-bad");

    // Rapid-fire 10 bad connections (connect + close)
    for _ in 0..10 {
        let stream = fix.connect();
        drop(stream);
    }

    std::thread::sleep(Duration::from_millis(300));
    assert!(fix.server_alive(), "server crashed under bad client flood");

    // A good client should still be able to query
    let pid = query_pid(&fix.socket_path());
    assert!(pid.is_some(), "server not responding after bad clients");
}

#[test]
fn server_invalid_intent_in_hello() {
    let fix = ServerFixture::start("bad-intent");
    let stream = fix.connect();

    // Craft a Hello with invalid intent=99
    let mut payload = vec![0u8; 8];
    payload[0] = mneme::protocol::PROTOCOL_VERSION;
    payload[1] = 99; // invalid intent
    let pkt = raw_packet(0, &payload); // type 0 = Hello
    (&stream).write_all(&pkt).expect("write failed");

    std::thread::sleep(Duration::from_millis(300));
    // Server should disconnect (can't parse Hello with invalid intent)
    assert!(fix.server_alive(), "server crashed on invalid intent");
}

#[test]
fn query_then_attach_same_session() {
    let fix = ServerFixture::start("query-then-attach");

    // First: query
    let pid = query_pid(&fix.socket_path());
    assert!(pid.is_some(), "query failed");

    // Second: attach (with immediate detach)
    let stream = fix.connect();
    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Attach,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(stream.as_fd(), &mneme::protocol::Packet::hello(&hello))
        .unwrap();

    // Should get Welcome
    let pkt = recv_typed(&stream).expect("no Welcome");
    assert_eq!(pkt.msg_type, mneme::protocol::MsgType::Welcome);

    // Send Detach
    mneme::protocol::send_packet(
        stream.as_fd(),
        &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
    )
    .unwrap();

    drop(stream);
    assert!(fix.server_alive(), "server died after query+attach");
}
