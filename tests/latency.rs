//! Latency and throughput profiling for mn.
//!
//! Measures the overhead mn adds to terminal I/O by running `cat` inside
//! a session and timing round-trips through the server.
//!
//! Run with: cargo test --test latency --release -- --nocapture

use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn mn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

fn start_cat_session(prefix: &str) -> (TempDir, String, u32) {
    let dir = TempDir::new().expect("tempdir");
    let session = format!("{prefix}-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["new", &session, "cat"])
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

fn attach_live(socket: &Path) -> UnixStream {
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

    // Drain Welcome + Replay* + ReplayEnd
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
// Latency: single-byte round-trip through cat
// ---------------------------------------------------------------------------

#[test]
fn profile_roundtrip_latency() {
    let (dir, session, pid) = start_cat_session("latency");
    let socket = dir.path().join(&session);
    let stream = attach_live(&socket);

    const WARMUP: usize = 50;
    const ITERATIONS: usize = 500;

    let mut latencies = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let byte = b'A' + (i as u8 % 26);

        let start = Instant::now();

        // Send one byte
        let pkt = mneme::protocol::Packet::content(&[byte]);
        mneme::protocol::send_packet(stream.as_fd(), &pkt).unwrap();

        // Wait for echo
        loop {
            let resp = mneme::protocol::recv_packet(stream.as_fd()).unwrap();
            if resp.msg_type == mneme::protocol::MsgType::Content
                && resp.payload.contains(&byte)
            {
                break;
            }
        }

        let elapsed = start.elapsed();

        if i >= WARMUP {
            latencies.push(elapsed);
        }
    }

    latencies.sort();

    let sum: Duration = latencies.iter().sum();
    let mean = sum / latencies.len() as u32;
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];
    let min = latencies[0];
    let max = latencies[latencies.len() - 1];

    eprintln!();
    eprintln!("=== Round-trip latency (single byte through cat) ===");
    eprintln!("  samples: {ITERATIONS} (after {WARMUP} warmup)");
    eprintln!("  mean:    {:.1}µs", mean.as_nanos() as f64 / 1000.0);
    eprintln!("  p50:     {:.1}µs", p50.as_nanos() as f64 / 1000.0);
    eprintln!("  p95:     {:.1}µs", p95.as_nanos() as f64 / 1000.0);
    eprintln!("  p99:     {:.1}µs", p99.as_nanos() as f64 / 1000.0);
    eprintln!("  min:     {:.1}µs", min.as_nanos() as f64 / 1000.0);
    eprintln!("  max:     {:.1}µs", max.as_nanos() as f64 / 1000.0);

    drop(stream);
    kill_server(pid);
}

// ---------------------------------------------------------------------------
// Throughput: large data transfer through cat
// ---------------------------------------------------------------------------

#[test]
fn profile_throughput() {
    // Measure one-way throughput: child writes data, client reads it.
    // Uses dd to generate exactly 1 MiB of output.
    let dir = TempDir::new().expect("tempdir");
    let session = format!("throughput-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["new", &session, "/bin/sh", "-c",
               "sleep 1; dd if=/dev/zero bs=65536 count=16 2>/dev/null; sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(output.status.success());
    std::thread::sleep(Duration::from_millis(300));

    let socket = dir.path().join(&session);
    let pid = query_pid(&socket).expect("query PID");
    let stream = attach_live(&socket);

    const TOTAL: usize = 65536 * 16; // 1 MiB
    let mut received = 0;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

    let start = Instant::now();

    while received < TOTAL {
        match mneme::protocol::recv_packet(stream.as_fd()) {
            Ok(pkt) if pkt.msg_type == mneme::protocol::MsgType::Content => {
                received += pkt.payload.len();
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("recv error at {received}/{TOTAL}: {e}");
                break;
            }
        }
    }

    let elapsed = start.elapsed();
    let mb_per_sec = (received as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();

    eprintln!();
    eprintln!("=== Throughput (1 MiB one-way, child → client) ===");
    eprintln!("  received:  {} bytes", received);
    eprintln!("  time:      {:.1}ms", elapsed.as_secs_f64() * 1000.0);
    eprintln!("  rate:      {:.1} MiB/s", mb_per_sec);

    drop(stream);
    kill_server(pid);
}

// ---------------------------------------------------------------------------
// Baseline: direct PTY latency without mn (for comparison)
// ---------------------------------------------------------------------------

#[test]
fn profile_baseline_pty_latency() {
    // Open a PTY directly and run cat, measure echo latency without mn in the path.
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
    assert_eq!(ret, 0, "openpty failed");

    let leader_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(leader) };
    let follower_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(follower) };

    // Spawn cat on the follower
    let follower_clone1 = follower_fd.try_clone().unwrap();
    let follower_clone2 = follower_fd.try_clone().unwrap();
    let mut child = Command::new("cat")
        .stdin(Stdio::from(follower_fd))
        .stdout(Stdio::from(follower_clone1))
        .stderr(Stdio::from(follower_clone2))
        .spawn()
        .expect("spawn cat");

    use std::os::fd::IntoRawFd;
    use std::io::{Read, Write};

    let mut leader_file = unsafe { std::fs::File::from_raw_fd(leader_fd.into_raw_fd()) };

    const WARMUP: usize = 50;
    const ITERATIONS: usize = 500;
    let mut latencies = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let byte = b'A' + (i as u8 % 26);

        let start = Instant::now();
        leader_file.write_all(&[byte]).unwrap();
        leader_file.flush().unwrap();

        let mut buf = [0u8; 1];
        leader_file.read_exact(&mut buf).unwrap();

        let elapsed = start.elapsed();

        if i >= WARMUP {
            latencies.push(elapsed);
        }
    }

    latencies.sort();
    let sum: Duration = latencies.iter().sum();
    let mean = sum / latencies.len() as u32;
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];

    eprintln!();
    eprintln!("=== Baseline PTY latency (cat, no mn) ===");
    eprintln!("  samples: {ITERATIONS} (after {WARMUP} warmup)");
    eprintln!("  mean:    {:.1}µs", mean.as_nanos() as f64 / 1000.0);
    eprintln!("  p50:     {:.1}µs", p50.as_nanos() as f64 / 1000.0);
    eprintln!("  p95:     {:.1}µs", p95.as_nanos() as f64 / 1000.0);
    eprintln!("  p99:     {:.1}µs", p99.as_nanos() as f64 / 1000.0);

    let _ = child.kill();
    let _ = child.wait();
}
