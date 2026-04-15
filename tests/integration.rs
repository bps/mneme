//! Integration tests for the mn binary.
//!
//! Each test spawns real server processes and interacts with them through
//! the CLI, verifying the full create → attach → replay → detach lifecycle.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn mn() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

/// Run `mn` with args, capture stdout+stderr, with a timeout.
fn mn_run(args: &[&str], timeout_secs: u64) -> CmdResult {
    mn_run_with_stdin(args, None, timeout_secs)
}

/// Run `mn` with args and optional stdin, capture stdout+stderr.
fn mn_run_with_stdin(args: &[&str], stdin_bytes: Option<&[u8]>, timeout_secs: u64) -> CmdResult {
    let mut cmd = Command::new(mn());
    cmd.args(args)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn mn");

    if let Some(input) = stdin_bytes {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).ok();
            drop(stdin); // close stdin so the process sees EOF
        }
    }

    let output = wait_with_timeout(&mut child, Duration::from_secs(timeout_secs));
    match output {
        Some(out) => CmdResult {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        },
        None => {
            let _ = child.kill();
            let _ = child.wait();
            CmdResult {
                code: -1,
                stdout: String::new(),
                stderr: "TIMEOUT".into(),
            }
        }
    }
}

/// Wait for a child process with timeout. Returns None on timeout.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::thread;

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
                return Some(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    return None;
                }
                thread::sleep(poll_interval);
            }
            Err(_) => return None,
        }
    }
}

struct CmdResult {
    code: i32,
    stdout: String,
    stderr: String,
}

/// RAII guard that cleans up a session on drop.
struct TestSession {
    name: String,
}

impl TestSession {
    fn new(prefix: &str) -> Self {
        // Unique name per test to avoid collisions
        let name = format!("{prefix}-{}", std::process::id());
        Self { name }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        // Kill server if running
        let _ = Command::new("pkill")
            .args(["-f", &format!("mn --server {}", self.name)])
            .output();
        // Small delay for cleanup
        std::thread::sleep(Duration::from_millis(100));
        // Remove socket
        if let Ok(home) = std::env::var("HOME") {
            let socket = PathBuf::from(home).join(".mneme").join(&self.name);
            let _ = std::fs::remove_file(socket);
        }
    }
}

const DETACH: &[u8] = b"\x1c"; // Ctrl-backslash

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn create_only_and_list() {
    let sess = TestSession::new("create-list");

    // Create without attaching
    let r = mn_run(&["-n", sess.name(), "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);
    assert!(r.stderr.contains("session created"), "stderr: {}", r.stderr);

    // Wait for server to be ready
    std::thread::sleep(Duration::from_millis(500));

    // List should show the session
    let r = mn_run(&[], 5);
    assert_eq!(r.code, 0, "list failed: {}", r.stderr);
    assert!(
        r.stdout.contains(sess.name()),
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
    let sess = TestSession::new("create-attach");

    // Create+attach: child echoes then sleeps, we detach after a delay.
    // Spawn mn, wait a moment for output, then send detach key.
    let mut child = Command::new(mn())
        .args(["-c", sess.name(), "/bin/sh", "-c", "echo HELLO_WORLD; sleep 60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    // Give the child time to produce output
    std::thread::sleep(Duration::from_secs(1));

    // Send detach key
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(DETACH).ok();
        drop(stdin);
    }

    let output = wait_with_timeout(&mut child, Duration::from_secs(5))
        .expect("timeout waiting for detach");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);

    assert_eq!(code, 0, "create+attach failed: {stderr}");
    assert!(
        stdout.contains("HELLO_WORLD"),
        "output missing: stdout='{stdout}'",
    );
    assert!(stderr.contains("detached"), "not detached: {stderr}");
}

#[test]
fn replay_on_reattach() {
    let sess = TestSession::new("replay");

    // Create+attach, child prints lines, detach immediately
    let r = mn_run_with_stdin(
        &[
            "-c",
            sess.name(),
            "/bin/sh",
            "-c",
            "echo LINE_ONE; echo LINE_TWO; echo LINE_THREE; sleep 60",
        ],
        Some(DETACH),
        5,
    );
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);

    // Small delay for ring buffer to capture output
    std::thread::sleep(Duration::from_millis(500));

    // Reattach — should see replayed output
    let r = mn_run_with_stdin(&["-a", sess.name()], Some(DETACH), 5);
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
    let sess = TestSession::new("fast-exit");

    let r = mn_run(&["-c", sess.name(), "/usr/bin/true"], 5);
    assert_eq!(r.code, 0, "fast exit failed: {}", r.stderr);
    assert!(
        r.stderr.contains("exit status 0"),
        "wrong exit message: {}",
        r.stderr
    );
}

#[test]
fn fast_exit_child_nonzero() {
    let sess = TestSession::new("fast-exit-nz");

    let r = mn_run(&["-c", sess.name(), "/usr/bin/false"], 5);
    assert_eq!(r.code, 1, "expected exit code 1, got {}: {}", r.code, r.stderr);
    assert!(
        r.stderr.contains("exit status 1"),
        "wrong exit message: {}",
        r.stderr
    );
}

#[test]
fn attach_or_create_new() {
    let sess = TestSession::new("aoc-new");

    // -A on nonexistent session should create+attach
    let r = mn_run_with_stdin(
        &["-A", sess.name(), "/bin/sh", "-c", "echo FRESH; sleep 60"],
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
    let sess = TestSession::new("aoc-exist");

    // Create first
    let r = mn_run(&["-n", sess.name(), "/bin/sh", "-c", "echo EXISTS; sleep 60"], 5);
    assert_eq!(r.code, 0, "create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(500));

    // -A should attach (not create)
    let r = mn_run_with_stdin(&["-A", sess.name()], Some(DETACH), 5);
    assert_eq!(r.code, 0, "-A existing failed: {}", r.stderr);
    assert!(
        !r.stderr.contains("session created"),
        "should not re-create: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("detached"),
        "should detach: {}",
        r.stderr
    );
}

#[test]
fn force_recreate() {
    let sess = TestSession::new("force");

    // Create session
    let r = mn_run(&["-n", sess.name(), "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "first create failed: {}", r.stderr);
    std::thread::sleep(Duration::from_millis(500));

    // Without -f, should fail
    let r = mn_run(
        &["-n", sess.name(), "/bin/sh", "-c", "sleep 60"],
        5,
    );
    assert_ne!(r.code, 0, "duplicate create should fail");
    assert!(
        r.stderr.contains("already exists"),
        "wrong error: {}",
        r.stderr
    );

    // With -f, should succeed
    let r = mn_run(
        &["-f", "-n", sess.name(), "/bin/sh", "-c", "sleep 60"],
        5,
    );
    assert_eq!(r.code, 0, "force create failed: {}", r.stderr);
}

#[test]
fn quiet_mode() {
    let sess = TestSession::new("quiet");

    let r = mn_run(&["-q", "-n", sess.name(), "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(r.code, 0, "quiet create failed: {}", r.stderr);
    assert!(
        r.stderr.is_empty(),
        "quiet mode should suppress stderr: '{}'",
        r.stderr
    );
}

#[test]
fn session_query_shows_child_status() {
    let sess = TestSession::new("query-status");

    // Create with a child that exits
    let r = mn_run(&["-c", sess.name(), "/usr/bin/true"], 5);
    assert!(
        r.stderr.contains("exit status 0"),
        "wrong exit: {}",
        r.stderr
    );

    // The server should have exited, session gone
    std::thread::sleep(Duration::from_millis(500));
    let r = mn_run(&[], 5);
    // Session should not appear in list (server exited after client was notified)
    assert!(
        !r.stdout.contains(sess.name()),
        "exited session should be gone: {}",
        r.stdout
    );
}

#[test]
fn invalid_session_name_rejected() {
    let r = mn_run(&["-c", "bad/name", "/bin/sh"], 5);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("alphanumeric"), "wrong error: {}", r.stderr);
}

#[test]
fn attach_nonexistent_fails() {
    let r = mn_run(&["-a", "does-not-exist-99999"], 5);
    assert_ne!(r.code, 0, "should fail: {}", r.stderr);
}

#[test]
fn mutual_exclusion() {
    let r = mn_run(&["-c", "-a", "test"], 5);
    assert_ne!(r.code, 0);
    assert!(
        r.stderr.contains("cannot be used with"),
        "wrong error: {}",
        r.stderr
    );
}

#[test]
fn missing_session_name() {
    let r = mn_run(&["-c"], 5);
    assert_ne!(r.code, 0);
    assert!(
        r.stderr.contains("session name"),
        "wrong error: {}",
        r.stderr
    );
}
