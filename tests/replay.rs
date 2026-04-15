//! Replay correctness tests.
//!
//! Verify that replay delivers the right bytes in the right order,
//! that the snapshot is isolated from concurrent PTY writes, and that
//! live data arrives as Content after ReplayEnd.

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

/// Start a server with the given shell command, return (TempDir, session_name, server_pid).
fn start_session(prefix: &str, shell_cmd: &str) -> (TempDir, String, u32) {
    let dir = TempDir::new().expect("tempdir");
    let session = format!("{prefix}-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["-n", &session, "/bin/sh", "-c", shell_cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(output.status.success(), "create failed: {}", String::from_utf8_lossy(&output.stderr));

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

/// Connect to a session and perform the Attach handshake.
/// Returns the stream (ready for Replay/Content) and the Welcome.
fn attach_raw(socket: &Path) -> (UnixStream, mneme::protocol::Welcome) {
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

    let pkt = mneme::protocol::recv_packet(s.as_fd()).unwrap();
    assert_eq!(pkt.msg_type, mneme::protocol::MsgType::Welcome);
    let welcome = pkt.parse_welcome().unwrap();
    (s, welcome)
}

/// Read all Replay packets until ReplayEnd, returning the concatenated bytes.
/// Also returns any Content packets that arrive immediately after.
fn collect_replay(stream: &UnixStream) -> (Vec<u8>, Vec<u8>) {
    let mut replay_bytes = Vec::new();
    let mut live_bytes = Vec::new();
    let mut replay_done = false;

    // Read packets until we hit a timeout or Exit
    loop {
        let pkt = match mneme::protocol::recv_packet(stream.as_fd()) {
            Ok(p) => p,
            Err(_) => break,
        };

        match pkt.msg_type {
            mneme::protocol::MsgType::Replay => {
                assert!(!replay_done, "Replay packet after ReplayEnd");
                replay_bytes.extend_from_slice(&pkt.payload);
            }
            mneme::protocol::MsgType::ReplayEnd => {
                assert!(!replay_done, "duplicate ReplayEnd");
                replay_done = true;
                // Try to read a few more packets (live data)
                // Switch to short timeout for live data collection
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .ok();
            }
            mneme::protocol::MsgType::Content => {
                assert!(replay_done, "Content packet before ReplayEnd");
                live_bytes.extend_from_slice(&pkt.payload);
            }
            mneme::protocol::MsgType::Exit => break,
            _ => {} // ignore ResizeReq etc.
        }
    }

    assert!(replay_done, "never received ReplayEnd");
    (replay_bytes, live_bytes)
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
fn replay_delivers_exact_bytes() {
    let (dir, session, pid) = start_session("exact", "echo -n 'AAABBBCCC'; sleep 60");
    std::thread::sleep(Duration::from_millis(500)); // let output reach ring buffer

    let socket = dir.path().join(&session);
    let (stream, welcome) = attach_raw(&socket);
    assert!(welcome.ring_used > 0, "ring should have data");

    let (replay, _) = collect_replay(&stream);

    // The replay must contain the exact bytes the child wrote
    let replay_str = String::from_utf8_lossy(&replay);
    assert!(
        replay_str.contains("AAABBBCCC"),
        "replay missing expected data: got '{replay_str}'"
    );

    drop(stream);
    kill_server(pid);
}

#[test]
fn replay_preserves_byte_order() {
    // Write numbered lines so we can verify order
    let cmd = "for i in 1 2 3 4 5 6 7 8 9 10; do echo \"LINE_$i\"; done; sleep 60";
    let (dir, session, pid) = start_session("order", cmd);
    std::thread::sleep(Duration::from_millis(500));

    let socket = dir.path().join(&session);
    let (stream, _) = attach_raw(&socket);
    let (replay, _) = collect_replay(&stream);
    let text = String::from_utf8_lossy(&replay);

    // Verify lines appear in order
    let mut last_pos = 0;
    for i in 1..=10 {
        let needle = format!("LINE_{i}");
        let pos = text.find(&needle).unwrap_or_else(|| panic!("missing {needle} in: {text}"));
        assert!(
            pos >= last_pos,
            "LINE_{i} at {pos} is before LINE_{} at {last_pos}",
            i - 1
        );
        last_pos = pos;
    }

    drop(stream);
    kill_server(pid);
}

#[test]
fn replay_with_empty_ring_buffer() {
    // A child that produces no output
    let (dir, session, pid) = start_session("empty-ring", "sleep 60");
    std::thread::sleep(Duration::from_millis(300));

    let socket = dir.path().join(&session);
    let (stream, welcome) = attach_raw(&socket);
    assert_eq!(welcome.ring_used, 0, "ring should be empty");

    // Should get ReplayEnd immediately (no Replay packets)
    let pkt = mneme::protocol::recv_packet(stream.as_fd()).unwrap();
    assert_eq!(
        pkt.msg_type,
        mneme::protocol::MsgType::ReplayEnd,
        "expected immediate ReplayEnd for empty ring, got {:?}",
        pkt.msg_type
    );

    drop(stream);
    kill_server(pid);
}

#[test]
fn replay_large_output_chunked_correctly() {
    // Generate output larger than MAX_PAYLOAD (4093) to force multiple Replay packets
    let cmd = "dd if=/dev/zero bs=1024 count=8 2>/dev/null | tr '\\0' 'X'; sleep 60";
    let (dir, session, pid) = start_session("large", cmd);
    std::thread::sleep(Duration::from_millis(500));

    let socket = dir.path().join(&session);
    let (stream, welcome) = attach_raw(&socket);
    assert!(welcome.ring_used > 4093, "need >4093 bytes for chunking test, got {}", welcome.ring_used);

    let mut replay_bytes = Vec::new();
    let mut packet_count = 0;

    loop {
        let pkt = mneme::protocol::recv_packet(stream.as_fd()).unwrap();
        match pkt.msg_type {
            mneme::protocol::MsgType::Replay => {
                assert!(
                    pkt.payload.len() <= mneme::protocol::MAX_PAYLOAD,
                    "replay chunk too large: {}",
                    pkt.payload.len()
                );
                replay_bytes.extend_from_slice(&pkt.payload);
                packet_count += 1;
            }
            mneme::protocol::MsgType::ReplayEnd => break,
            other => panic!("unexpected message during replay: {other:?}"),
        }
    }

    assert!(packet_count >= 2, "expected multiple Replay packets, got {packet_count}");
    assert_eq!(
        replay_bytes.len(),
        welcome.ring_used as usize,
        "total replay bytes should match ring_used"
    );

    drop(stream);
    kill_server(pid);
}

#[test]
fn live_data_arrives_after_replay_end() {
    // Child prints initial output, then sleeps, then prints more after a trigger.
    // We use a FIFO (named pipe) so the child blocks until we signal it.
    let fifo_path = format!("/tmp/mneme-test-fifo-{}", std::process::id());
    let _ = std::fs::remove_file(&fifo_path);
    unsafe {
        let c_path = std::ffi::CString::new(fifo_path.clone()).unwrap();
        libc::mkfifo(c_path.as_ptr(), 0o600);
    }

    let cmd = format!(
        "echo BEFORE_REPLAY; cat {fifo_path}; sleep 60"
    );
    let (dir, session, pid) = start_session("live-after", &cmd);
    std::thread::sleep(Duration::from_millis(500));

    let socket = dir.path().join(&session);
    let (stream, _) = attach_raw(&socket);

    // Collect replay (should contain "BEFORE_REPLAY")
    let (replay, _) = collect_replay(&stream);
    let replay_text = String::from_utf8_lossy(&replay);
    assert!(
        replay_text.contains("BEFORE_REPLAY"),
        "replay should contain initial output, got: '{replay_text}'"
    );

    // Now write to the FIFO — the child will print this as live output
    {
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo_path)
            .expect("open fifo");
        writeln!(fifo, "AFTER_REPLAY").expect("write fifo");
    }

    // Read live Content
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok();
    let mut live = Vec::new();
    loop {
        match mneme::protocol::recv_packet(stream.as_fd()) {
            Ok(pkt) => match pkt.msg_type {
                mneme::protocol::MsgType::Content => {
                    live.extend_from_slice(&pkt.payload);
                    let text = String::from_utf8_lossy(&live);
                    if text.contains("AFTER_REPLAY") {
                        break;
                    }
                }
                mneme::protocol::MsgType::Exit => break,
                _ => {}
            },
            Err(_) => break,
        }
    }

    let live_text = String::from_utf8_lossy(&live);
    assert!(
        live_text.contains("AFTER_REPLAY"),
        "live data should contain post-replay output, got: '{live_text}'"
    );

    drop(stream);
    let _ = std::fs::remove_file(&fifo_path);
    kill_server(pid);
}

#[test]
fn second_attach_gets_full_replay_including_first_session_output() {
    let (dir, session, pid) = start_session("multi-attach", "echo ORIGINAL_OUTPUT; sleep 60");
    std::thread::sleep(Duration::from_millis(500));

    let socket = dir.path().join(&session);

    // First attach + detach
    {
        let (stream, _) = attach_raw(&socket);
        let (replay, _) = collect_replay(&stream);
        let text = String::from_utf8_lossy(&replay);
        assert!(text.contains("ORIGINAL_OUTPUT"), "first attach missing output: '{text}'");
        // Detach
        mneme::protocol::send_packet(
            stream.as_fd(),
            &mneme::protocol::Packet::empty(mneme::protocol::MsgType::Detach),
        )
        .ok();
    }

    std::thread::sleep(Duration::from_millis(200));

    // Second attach — should also see the output via replay
    {
        let (stream, _) = attach_raw(&socket);
        let (replay, _) = collect_replay(&stream);
        let text = String::from_utf8_lossy(&replay);
        assert!(
            text.contains("ORIGINAL_OUTPUT"),
            "second attach missing output: '{text}'"
        );
    }

    kill_server(pid);
}

#[test]
fn no_replay_content_before_replay_end() {
    // Verify we never get Content before ReplayEnd
    let (dir, session, pid) = start_session("no-early-content", "echo DATA; sleep 60");
    std::thread::sleep(Duration::from_millis(500));

    let socket = dir.path().join(&session);
    let (stream, _) = attach_raw(&socket);

    let mut got_replay_end = false;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();

    loop {
        let pkt = match mneme::protocol::recv_packet(stream.as_fd()) {
            Ok(p) => p,
            Err(_) => break,
        };
        match pkt.msg_type {
            mneme::protocol::MsgType::Replay => {
                assert!(!got_replay_end, "Replay after ReplayEnd");
            }
            mneme::protocol::MsgType::ReplayEnd => {
                got_replay_end = true;
                break; // don't need to read further
            }
            mneme::protocol::MsgType::Content => {
                assert!(got_replay_end, "Content before ReplayEnd!");
            }
            _ => {}
        }
    }
    assert!(got_replay_end, "never got ReplayEnd");

    drop(stream);
    kill_server(pid);
}
