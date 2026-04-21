//! Integration tests for the mn binary.
//!
//! Each test gets an isolated socket directory (via MNEME_SOCKET_DIR) in a
//! temp dir, so tests can run in parallel without interfering with each
//! other or the user's real sessions.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn mn_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Isolated test environment with its own socket directory.
/// On drop, kills all server processes and removes the dir.
struct TestEnv {
    dir: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("failed to create temp dir"),
        }
    }

    /// Path to the socket directory.
    fn socket_dir(&self) -> &Path {
        self.dir.path()
    }

    /// Generate a unique session name (includes PID + counter for uniqueness).
    fn session(&self, prefix: &str) -> String {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{n}", std::process::id())
    }

    /// Build a Command for mn with MNEME_SOCKET_DIR set to our temp dir.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(mn_bin());
        cmd.env("MNEME_SOCKET_DIR", self.socket_dir());
        cmd
    }

    /// Run mn synchronously, capture output, enforce timeout.
    fn run(&self, args: &[&str], timeout_secs: u64) -> CmdResult {
        self.run_with_stdin(args, None, timeout_secs)
    }

    /// Run mn with optional stdin bytes, capture output.
    fn run_with_stdin(
        &self,
        args: &[&str],
        stdin_bytes: Option<&[u8]>,
        timeout_secs: u64,
    ) -> CmdResult {
        let mut cmd = self.cmd();
        cmd.args(args)
            .stdin(if stdin_bytes.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to spawn mn");

        if let Some(input) = stdin_bytes
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(input).ok();
            drop(stdin);
        }

        collect_output(&mut child, Duration::from_secs(timeout_secs))
    }

    /// Spawn mn as a background process (for interactive tests).
    fn spawn(&self, args: &[&str]) -> Child {
        self.cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn mn")
    }

    /// Query a session's server PID by connecting to its socket.
    fn server_pid(&self, session: &str) -> Option<u32> {
        let path = self.socket_dir().join(session);
        let stream = UnixStream::connect(&path).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;

        // Send a Query Hello
        let hello = mneme::protocol::Hello {
            version: mneme::protocol::PROTOCOL_VERSION,
            intent: mneme::protocol::Intent::Query,
            flags: mneme::protocol::ClientFlags::empty(),
            rows: 0,
            cols: 0,
        };
        mneme::protocol::send_packet(stream.as_fd(), &mneme::protocol::Packet::hello(&hello))
            .ok()?;

        let pkt = mneme::protocol::recv_packet(stream.as_fd()).ok()?;
        let welcome = pkt.parse_welcome()?;
        Some(welcome.server_pid)
    }

    /// Kill all servers with sockets in our directory.
    fn kill_all_servers(&self) {
        // Discover current PIDs
        let entries = match std::fs::read_dir(self.socket_dir()) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut pids = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(pid) = self.server_pid(&name) {
                pids.push(pid);
            }
        }
        if pids.is_empty() {
            return;
        }
        // SIGKILL everything immediately — no graceful shutdown in tests
        for &pid in &pids {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        // Wait for them to die
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let any_alive = pids
                .iter()
                .any(|&pid| unsafe { libc::kill(pid as libc::pid_t, 0) == 0 });
            if !any_alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.kill_all_servers();
    }
}

/// Collected output from a command.
struct CmdResult {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Wait for a child to exit, with timeout. Kills on timeout.
fn collect_output(child: &mut Child, timeout: Duration) -> CmdResult {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    out.read_to_end(&mut stdout).ok();
                }
                if let Some(mut err) = child.stderr.take() {
                    err.read_to_end(&mut stderr).ok();
                }
                return CmdResult {
                    code: status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                };
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CmdResult {
                        code: -1,
                        stdout: String::new(),
                        stderr: "TIMEOUT".into(),
                    };
                }
                std::thread::sleep(poll_interval);
            }
            Err(_) => {
                return CmdResult {
                    code: -1,
                    stdout: String::new(),
                    stderr: "wait error".into(),
                };
            }
        }
    }
}

const DETACH: &[u8] = b"\x1c";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn create_only_and_list() {
    let env = TestEnv::new();
    let sess = env.session("create-list");

    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);
    assert!(r.stderr.contains("session created"), "stderr: {}", r.stderr);

    std::thread::sleep(Duration::from_millis(500));

    let r = env.run(&[], 5);
    assert_eq!(r.code, 0, "list failed: {}", r.stderr);
    assert!(
        r.stdout.contains(&sess),
        "session not in list: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("Active sessions"),
        "missing header: {}",
        r.stdout
    );
}

#[test]
fn create_attach_detach() {
    let env = TestEnv::new();
    let sess = env.session("create-attach");

    // Spawn mn -c, wait for output, then send detach key
    let mut child = env.spawn(&[
        "create",
        &sess,
        "/bin/sh",
        "-c",
        "echo HELLO_WORLD; sleep 60",
    ]);

    std::thread::sleep(Duration::from_secs(1));

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(DETACH).ok();
        drop(stdin);
    }

    let r = collect_output(&mut child, Duration::from_secs(5));
    assert_eq!(r.code, 0, "create+attach failed: {}", r.stderr);
    assert!(
        r.stdout.contains("HELLO_WORLD"),
        "output missing: stdout='{}'",
        r.stdout
    );
    assert!(r.stderr.contains("detached"), "not detached: {}", r.stderr);
}

#[test]
fn replay_on_reattach() {
    let env = TestEnv::new();
    let sess = env.session("replay");

    let r = env.run_with_stdin(
        &[
            "create",
            &sess,
            "/bin/sh",
            "-c",
            "echo LINE_ONE; echo LINE_TWO; echo LINE_THREE; sleep 60",
        ],
        Some(DETACH),
        5,
    );
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);

    std::thread::sleep(Duration::from_millis(500));

    let r = env.run_with_stdin(&["attach", &sess], Some(DETACH), 5);
    assert_eq!(r.code, 0, "reattach failed: {}", r.stderr);
    assert!(
        r.stdout.contains("LINE_ONE"),
        "replay missing LINE_ONE: stdout='{}'",
        r.stdout
    );
    assert!(
        r.stdout.contains("LINE_TWO"),
        "replay missing LINE_TWO: stdout='{}'",
        r.stdout
    );
    assert!(
        r.stdout.contains("LINE_THREE"),
        "replay missing LINE_THREE: stdout='{}'",
        r.stdout
    );
}

#[test]
fn fast_exit_child() {
    let env = TestEnv::new();
    let sess = env.session("fast-exit");

    let r = env.run(&["create", &sess, "/usr/bin/true"], 5);
    assert_eq!(r.code, 0, "fast exit failed: {}", r.stderr);
    assert!(
        r.stderr.contains("exit status 0"),
        "wrong exit message: {}",
        r.stderr
    );
}

#[test]
fn fast_exit_child_nonzero() {
    let env = TestEnv::new();
    let sess = env.session("fast-exit-nz");

    let r = env.run(&["create", &sess, "/usr/bin/false"], 5);
    assert_eq!(
        r.code, 1,
        "expected exit code 1, got {}: {}",
        r.code, r.stderr
    );
    assert!(
        r.stderr.contains("exit status 1"),
        "wrong exit message: {}",
        r.stderr
    );
}

#[test]
fn attach_or_create_new() {
    let env = TestEnv::new();
    let sess = env.session("aoc-new");

    let r = env.run_with_stdin(
        &["auto", &sess, "/bin/sh", "-c", "echo FRESH; sleep 60"],
        Some(DETACH),
        5,
    );
    assert_eq!(r.code, 0, "-A new failed: {}", r.stderr);
    assert!(
        r.stderr.contains("session created"),
        "missing created msg: {}",
        r.stderr
    );
}

#[test]
fn attach_or_create_existing() {
    let env = TestEnv::new();
    let sess = env.session("aoc-exist");

    let r = env.run(&["new", &sess, "/bin/sh", "-c", "echo EXISTS; sleep 60"], 5);
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(500));

    let r = env.run_with_stdin(&["auto", &sess], Some(DETACH), 5);
    assert_eq!(r.code, 0, "-A existing failed: {}", r.stderr);
    assert!(
        !r.stderr.contains("session created"),
        "should not re-create: {}",
        r.stderr
    );
    assert!(r.stderr.contains("detached"), "should detach: {}", r.stderr);
}

#[test]
fn force_recreate() {
    let env = TestEnv::new();
    let sess = env.session("force");

    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "first create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(500));

    // Without -f, should fail
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_ne!(r.code, 0, "duplicate create should fail");
    assert!(
        r.stderr.contains("already exists"),
        "wrong error: {}",
        r.stderr
    );

    // With -f, should succeed
    let r = env.run(&["new", "-f", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "force create failed: {}", r.stderr);
}

#[test]
fn quiet_mode() {
    let env = TestEnv::new();
    let sess = env.session("quiet");

    let r = env.run(&["new", "-q", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "quiet create failed: {}", r.stderr);
    assert!(
        r.stderr.is_empty(),
        "quiet mode should suppress stderr: '{}'",
        r.stderr
    );
}

#[test]
fn session_cleanup_after_exit() {
    let env = TestEnv::new();
    let sess = env.session("cleanup");

    let r = env.run(&["create", &sess, "/usr/bin/true"], 5);
    assert!(
        r.stderr.contains("exit status 0"),
        "wrong exit: {}",
        r.stderr
    );

    // Server should have exited — session gone from list
    std::thread::sleep(Duration::from_millis(500));
    let r = env.run(&[], 5);
    assert!(
        !r.stdout.contains(&sess),
        "exited session should be gone: {}",
        r.stdout
    );
}

#[test]
fn invalid_session_name_rejected() {
    let env = TestEnv::new();
    let r = env.run(&["create", "bad/name", "/bin/sh"], 5);
    assert_ne!(r.code, 0);
    assert!(
        r.stderr.contains("alphanumeric"),
        "wrong error: {}",
        r.stderr
    );
}

#[test]
fn attach_nonexistent_fails() {
    let env = TestEnv::new();
    let r = env.run(&["attach", "does-not-exist-99999"], 5);
    assert_ne!(r.code, 0, "should fail: {}", r.stderr);
}

#[test]
fn unknown_subcommand_rejected() {
    let env = TestEnv::new();
    let r = env.run(&["bogus"], 5);
    assert_ne!(r.code, 0);
    assert!(
        r.stderr.contains("invalid") || r.stderr.contains("unrecognized"),
        "wrong error: {}",
        r.stderr
    );
}

#[test]
fn missing_session_name() {
    let env = TestEnv::new();
    let r = env.run(&["create"], 5);
    assert_ne!(r.code, 0);
    // clap reports this as a missing required argument
    assert!(
        r.stderr.contains("<NAME>") || r.stderr.contains("required"),
        "wrong error: {}",
        r.stderr
    );
}

#[test]
fn server_log_captures_server_stderr_on_spawn_failure() {
    let env = TestEnv::new();
    let sess = env.session("server-log");
    let log_path = env.socket_dir().join("server.log");

    let mut cmd = env.cmd();
    cmd.env("MNEME_SERVER_LOG", &log_path)
        .args(["create", &sess, "/definitely/missing/binary"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn mn");
    let r = collect_output(&mut child, Duration::from_secs(5));

    assert_ne!(r.code, 0, "create should fail");

    let log = std::fs::read_to_string(&log_path).expect("server log missing");
    assert!(
        log.contains("mneme: server error:"),
        "missing server error prefix: {}",
        log
    );
    assert!(
        log.contains("spawn") || log.contains("/definitely/missing/binary"),
        "missing spawn details: {}",
        log
    );
}

// ---------------------------------------------------------------------------
// Stale session cleanup via lock file
// ---------------------------------------------------------------------------

/// When a server crashes (SIGKILL, OOM, etc.), its socket and lock files
/// are left on disk. The next `mn new <name>` must recognize the session
/// is dead (via the released flock) and transparently recover, rather
/// than erroring with "session already exists".
#[test]
fn create_after_server_killed_recovers() {
    let env = TestEnv::new();
    let sess = env.session("crashed");

    // Create a real session and hard-kill its server (simulating a crash).
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "first create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(300));

    let pid = env.server_pid(&sess).expect("server PID not found");
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }

    // Wait for the kernel to reap and release the lock. flock release is
    // synchronous on process exit, so this is usually immediate.
    std::thread::sleep(Duration::from_millis(200));

    // Socket and lock files should still be on disk (no one cleaned up).
    let sock = env.socket_dir().join(&sess);
    let lock = env.socket_dir().join(format!("{sess}.lock"));
    assert!(sock.exists(), "socket should remain after SIGKILL");
    assert!(lock.exists(), "lock file should remain after SIGKILL");

    // Creating again should transparently recover — no -f needed.
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(
        r.code, 0,
        "stale-session recovery failed: stderr={}",
        r.stderr
    );

    // And the new server should be a different process.
    std::thread::sleep(Duration::from_millis(300));
    let new_pid = env.server_pid(&sess).expect("new server PID not found");
    assert_ne!(new_pid, pid, "expected a fresh server after recovery");
}

/// A live session must NOT be considered stale — creating with the same
/// name without -f should still fail, even with the new lock-based check.
#[test]
fn create_against_live_session_still_fails() {
    let env = TestEnv::new();
    let sess = env.session("live-conflict");

    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "first create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(300));

    // Duplicate create with live server must fail.
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_ne!(
        r.code, 0,
        "duplicate create against live session should fail"
    );
    assert!(
        r.stderr.contains("already exists"),
        "expected 'already exists', got: {}",
        r.stderr
    );
}

/// An orphan lock file (regular file only, no socket) with no holder
/// should be treated as stale and cleaned up on create.
#[test]
fn orphan_lock_file_is_cleaned() {
    let env = TestEnv::new();
    let sess = env.session("orphan-lock");

    // Plant an orphan lock file — regular file, no one holds flock on it.
    let lock = env.socket_dir().join(format!("{sess}.lock"));
    std::fs::write(&lock, b"").expect("write lock file");
    assert!(lock.exists());

    // Creating should succeed — no conflicting live server.
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "create with orphan lock failed: {}", r.stderr);
}

/// After normal session termination (child exits, last client detaches),
/// both socket and lock files should be removed.
#[test]
fn files_cleaned_after_clean_exit() {
    let env = TestEnv::new();
    let sess = env.session("clean-exit");

    // Short-lived child exits immediately; no clients ever attach.
    let r = env.run(&["new", &sess, "/bin/sh", "-c", "true"], 5);
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);

    // The server lingers until a client attaches to pick up the exit
    // status, so we force a kill to end it. That kill path also
    // cleans up both files.
    let _ = env.run(&["kill", &sess], 5);

    let sock = env.socket_dir().join(&sess);
    let lock = env.socket_dir().join(format!("{sess}.lock"));
    // Give the server a moment to complete its cleanup.
    for _ in 0..20 {
        if !sock.exists() && !lock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!sock.exists(), "socket file not cleaned up after kill");
    assert!(!lock.exists(), "lock file not cleaned up after kill");
}
