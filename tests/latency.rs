//! Latency and throughput profiling for mn.
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

fn print_stats(label: &str, unit: &str, values: &mut [f64]) {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len();
    let mean: f64 = values.iter().sum::<f64>() / n as f64;
    let p50 = values[n / 2];
    let p95 = values[n * 95 / 100];
    let p99 = values[n * 99 / 100];
    let min = values[0];
    let max = values[n - 1];

    eprintln!();
    eprintln!("=== {label} ===");
    eprintln!("  samples: {n}");
    eprintln!("  mean:    {mean:.1}{unit}");
    eprintln!("  p50:     {p50:.1}{unit}");
    eprintln!("  p95:     {p95:.1}{unit}");
    eprintln!("  p99:     {p99:.1}{unit}");
    eprintln!("  min:     {min:.1}{unit}");
    eprintln!("  max:     {max:.1}{unit}");
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

    let mut latencies_us = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let byte = b'A' + (i as u8 % 26);

        let start = Instant::now();

        let pkt = mneme::protocol::Packet::content(&[byte]);
        mneme::protocol::send_packet(stream.as_fd(), &pkt).unwrap();

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
            latencies_us.push(elapsed.as_nanos() as f64 / 1000.0);
        }
    }

    print_stats(
        "Round-trip latency (single byte through cat)",
        "µs",
        &mut latencies_us,
    );

    drop(stream);
    kill_server(pid);
}

// ---------------------------------------------------------------------------
// Throughput: child → client through mn
// ---------------------------------------------------------------------------

#[test]
fn profile_throughput() {
    // Child produces continuous output. We take multiple 1 MiB windows,
    // each timed independently, and report stats.
    let dir = TempDir::new().expect("tempdir");
    let session = format!("throughput-{}", std::process::id());

    let output = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(["new", &session, "/bin/sh", "-c",
               "sleep 2; while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done"])
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
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    const WINDOW: usize = 1024 * 1024; // 1 MiB per measurement
    const WARMUP: usize = 2;
    const ITERATIONS: usize = 10;

    let mut rates = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let mut received = 0;
        let start = Instant::now();

        while received < WINDOW {
            match mneme::protocol::recv_packet(stream.as_fd()) {
                Ok(pkt) if pkt.msg_type == mneme::protocol::MsgType::Content => {
                    received += pkt.payload.len();
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("recv error at {received}: {e}");
                    break;
                }
            }
        }

        let elapsed = start.elapsed();
        let mb_per_sec = (received as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();

        if i >= WARMUP {
            rates.push(mb_per_sec);
        }
    }

    print_stats(
        "Throughput (1 MiB windows, child → client)",
        " MiB/s",
        &mut rates,
    );

    drop(stream);
    kill_server(pid);
}

// ---------------------------------------------------------------------------
// Baseline: direct PTY latency without mn
// ---------------------------------------------------------------------------

#[test]
fn profile_baseline_pty_latency() {
    let mut leader: libc::c_int = -1;
    let mut follower: libc::c_int = -1;
    assert_eq!(0, unsafe {
        libc::openpty(
            &mut leader, &mut follower,
            std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
        )
    });

    let leader_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(leader) };
    let follower_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(follower) };

    let fc1 = follower_fd.try_clone().unwrap();
    let fc2 = follower_fd.try_clone().unwrap();
    let mut child = Command::new("cat")
        .stdin(Stdio::from(follower_fd))
        .stdout(Stdio::from(fc1))
        .stderr(Stdio::from(fc2))
        .spawn()
        .expect("spawn cat");

    use std::os::fd::IntoRawFd;
    use std::io::{Read, Write};

    let mut leader_file = unsafe { std::fs::File::from_raw_fd(leader_fd.into_raw_fd()) };

    const WARMUP: usize = 50;
    const ITERATIONS: usize = 500;
    let mut latencies_us = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let byte = b'A' + (i as u8 % 26);

        let start = Instant::now();
        leader_file.write_all(&[byte]).unwrap();
        leader_file.flush().unwrap();

        let mut buf = [0u8; 1];
        leader_file.read_exact(&mut buf).unwrap();

        let elapsed = start.elapsed();
        if i >= WARMUP {
            latencies_us.push(elapsed.as_nanos() as f64 / 1000.0);
        }
    }

    print_stats(
        "Baseline PTY latency (cat, no mn)",
        "µs",
        &mut latencies_us,
    );

    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Baseline: direct PTY throughput without mn
// ---------------------------------------------------------------------------

#[test]
fn profile_baseline_pty_throughput() {
    // Run dd in a loop, take multiple 1 MiB windows.
    let mut leader: libc::c_int = -1;
    let mut follower: libc::c_int = -1;
    assert_eq!(0, unsafe {
        libc::openpty(
            &mut leader, &mut follower,
            std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
        )
    });

    let leader_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(leader) };
    let follower_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(follower) };

    let fc1 = follower_fd.try_clone().unwrap();
    let fc2 = follower_fd.try_clone().unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done"])
        .stdin(Stdio::from(follower_fd))
        .stdout(Stdio::from(fc1))
        .stderr(Stdio::from(fc2))
        .spawn()
        .expect("spawn dd");

    use std::io::Read;
    use std::os::fd::{AsRawFd, IntoRawFd};

    unsafe {
        let flags = libc::fcntl(leader_fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(leader_fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let raw_fd = leader_fd.as_raw_fd();
    let mut leader_file = unsafe { std::fs::File::from_raw_fd(leader_fd.into_raw_fd()) };

    const WINDOW: usize = 1024 * 1024; // 1 MiB per measurement
    const WARMUP: usize = 2;
    const ITERATIONS: usize = 10;
    let mut buf = [0u8; 65536];
    let mut rates = Vec::with_capacity(ITERATIONS);

    for i in 0..(WARMUP + ITERATIONS) {
        let mut received = 0;
        let start = Instant::now();

        while received < WINDOW {
            let mut pfd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 5000) } <= 0 {
                break;
            }
            match leader_file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }

        let elapsed = start.elapsed();
        let mb_per_sec = (received as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();

        if i >= WARMUP {
            rates.push(mb_per_sec);
        }
    }

    print_stats(
        "Baseline PTY throughput (1 MiB windows, no mn)",
        " MiB/s",
        &mut rates,
    );

    let _ = child.kill();
    let _ = child.wait();
}
