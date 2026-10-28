//! Startup regressions. Each CLI command and protocol read has a deadline;
//! socket directories and cleanup are restricted to this test's own sessions.

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use mneme::protocol::{self, ClientFlags, Hello, Intent, PROTOCOL_VERSION, Packet};
use tempfile::TempDir;

struct Env(TempDir);

impl Env {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .tempdir()
                .unwrap(),
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mn"))
            .env("MNEME_SOCKET_DIR", self.0.path())
            .env_remove("MNEME_SERVER_LOG")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_end(&mut stdout)
                    .unwrap();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_end(&mut stderr)
                    .unwrap();
                return Output {
                    status,
                    stdout,
                    stderr,
                };
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("CLI command timed out: {args:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn query(&self) -> std::io::Result<protocol::Welcome> {
        let stream = UnixStream::connect(self.0.path().join("startup"))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent: Intent::Query,
                flags: ClientFlags::empty(),
                rows: 24,
                cols: 80,
            }),
        )?;
        protocol::recv_packet(stream.as_fd())?
            .parse_welcome()
            .ok_or_else(|| std::io::Error::other("expected Welcome"))
    }

    fn assert_failure(&self, args: &[&str], expected: &str) {
        let output = self.run(args);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "unexpected success: {err}");
        assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
        assert!(!err.contains("session created"), "false success: {err}");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Only this test's server, discovered through its private socket.
        // Successful tests use /usr/bin/true, so no persistent child remains.
        if let Ok(welcome) = self.query() {
            unsafe {
                libc::kill(welcome.server_pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[test]
fn missing_executable_reports_spawn_error_not_success() {
    let env = Env::new();
    let missing = env.0.path().join("no-such-executable");
    env.assert_failure(
        &["new", "startup", missing.to_str().unwrap()],
        "server startup failed: spawn",
    );
}

#[test]
fn socket_bind_failure_reaches_cli_without_server_logging() {
    let env = Env::new();
    // A directory cannot be removed by remove_file or bound as a socket.
    std::fs::create_dir(env.0.path().join("startup")).unwrap();
    env.assert_failure(
        &["new", "startup", "/usr/bin/true"],
        "server startup failed: bind",
    );
}

#[test]
fn zero_ring_sizes_are_rejected_before_startup() {
    for size in ["0", "0K", "0M"] {
        let env = Env::new();
        env.assert_failure(
            &["new", "-s", size, "startup", "/usr/bin/true"],
            "ring size must be between",
        );
        assert!(!env.0.path().join(".locks/startup").exists());
    }
}

#[test]
fn overflowing_size_multiplication_is_rejected() {
    for suffix in ["K", "M"] {
        let env = Env::new();
        let size = format!("{}{suffix}", usize::MAX);
        env.assert_failure(
            &["new", "-s", &size, "startup", "/usr/bin/true"],
            "size overflow",
        );
    }
}

#[test]
fn sizes_larger_than_protocol_capacity_are_rejected() {
    let env = Env::new();
    // Also exceeds usize on 32-bit platforms, where integer parsing rejects it.
    let output = env.run(&["new", "-s", "4294967296", "startup", "/usr/bin/true"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("ring size must be between") || err.contains("invalid size"),
        "{err}"
    );
    assert!(!env.0.path().join(".locks/startup").exists());
}

#[test]
fn successful_startup_is_immediately_queryable_with_requested_capacity() {
    let env = Env::new();
    let output = env.run(&["new", "-s", "8K", "startup", "/usr/bin/true"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // No retry or readiness sleep: CLI success is the synchronization barrier.
    let welcome = env.query().expect("query immediately after success");
    assert_eq!(welcome.ring_size, 8192);
    assert_eq!(welcome.version, PROTOCOL_VERSION);
}
