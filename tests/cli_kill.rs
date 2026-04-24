//! Integration tests for `mn kill <name>` and `mn kill all`.
//!
//! These exercise the live-server kill path (query → SIGTERM → wait for
//! lock release), the stale-cleanup branch, the orphan-lock sweep in
//! kill-all, and the not-found error branch.

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tempfile::TempDir;

fn mn_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn session(&self, prefix: &str) -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{n}", std::process::id())
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(mn_bin());
        c.env("MNEME_SOCKET_DIR", self.dir());
        c
    }

    fn run(&self, args: &[&str], timeout_secs: u64) -> (i32, String, String) {
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        wait_with_timeout(&mut child, Duration::from_secs(timeout_secs))
    }

    fn server_pid(&self, session: &str) -> Option<u32> {
        let path = self.dir().join(session);
        let stream = UnixStream::connect(&path).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
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
        Some(pkt.parse_welcome()?.server_pid)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best-effort cleanup of any servers we may have left behind.
        if let Ok(entries) = std::fs::read_dir(self.dir()) {
            let mut pids = Vec::new();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(pid) = self.server_pid(&name) {
                    pids.push(pid);
                }
            }
            for pid in pids {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> (i32, String, String) {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut s) = child.stdout.take() {
                    s.read_to_string(&mut out).ok();
                }
                if let Some(mut s) = child.stderr.take() {
                    s.read_to_string(&mut err).ok();
                }
                return (status.code().unwrap_or(-1), out, err);
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("mn timed out");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

// ---------------------------------------------------------------------------
// kill <live-session>
// ---------------------------------------------------------------------------

#[test]
fn kill_live_session_terminates_server_and_cleans_files() {
    let env = Env::new();
    let sess = env.session("kill-live");

    // Start a long-lived session.
    let (code, _, err) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0, "new failed: {err}");

    // Wait for server to be queryable.
    assert!(
        wait_for(Duration::from_secs(2), || env.server_pid(&sess).is_some()),
        "server never became queryable"
    );
    let pid = env.server_pid(&sess).expect("server pid");

    // Issue kill.
    let (code, _, err) = env.run(&["kill", &sess], 5);
    assert_eq!(code, 0, "kill failed: {err}");
    assert!(err.contains("killed"), "expected 'killed' in stderr: {err}");

    // Server process must actually be gone.
    assert!(
        wait_for(Duration::from_secs(2), || unsafe {
            libc::kill(pid as libc::pid_t, 0) != 0
        }),
        "server pid {pid} still alive after kill"
    );

    // Both files cleaned up.
    let sock = env.dir().join(&sess);
    let lock = env.dir().join(format!("{sess}.lock"));
    assert!(!sock.exists(), "socket not cleaned");
    assert!(!lock.exists(), "lock not cleaned");
}

#[test]
fn kill_quiet_suppresses_stderr() {
    let env = Env::new();
    let sess = env.session("kill-quiet");

    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0);
    assert!(wait_for(Duration::from_secs(2), || env
        .server_pid(&sess)
        .is_some()));

    let (code, _, err) = env.run(&["kill", "-q", &sess], 5);
    assert_eq!(code, 0, "kill -q failed: {err}");
    assert!(err.is_empty(), "kill -q should be silent, got: {err}");
}

// ---------------------------------------------------------------------------
// kill <nonexistent>
// ---------------------------------------------------------------------------

#[test]
fn kill_nonexistent_session_errors() {
    let env = Env::new();
    let (code, _, err) = env.run(&["kill", "no-such-session-xyz"], 5);
    assert_ne!(code, 0, "kill of nonexistent should fail");
    assert!(
        err.contains("not found"),
        "expected 'not found', got: {err}"
    );
}

// ---------------------------------------------------------------------------
// kill <stale-but-still-on-disk>
// ---------------------------------------------------------------------------

#[test]
fn kill_stale_session_cleans_files_and_succeeds() {
    let env = Env::new();
    let sess = env.session("kill-stale");

    // Create a real session, then SIGKILL the server so files remain.
    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0);
    assert!(wait_for(Duration::from_secs(2), || env
        .server_pid(&sess)
        .is_some()));
    let pid = env.server_pid(&sess).expect("pid");
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    // Wait for kernel to release the flock.
    assert!(wait_for(Duration::from_secs(2), || unsafe {
        libc::kill(pid as libc::pid_t, 0) != 0
    }));

    // Files should still be on disk (no graceful cleanup happened).
    let sock = env.dir().join(&sess);
    let lock = env.dir().join(format!("{sess}.lock"));
    assert!(sock.exists() || lock.exists(), "expected stale files");

    // kill should detect the stale session and clean it up.
    let (code, _, err) = env.run(&["kill", &sess], 5);
    assert_eq!(code, 0, "kill of stale failed: {err}");
    assert!(
        err.contains("stale") || err.contains("killed"),
        "expected stale/killed message, got: {err}"
    );
    assert!(!sock.exists(), "socket not cleaned");
    assert!(!lock.exists(), "lock not cleaned");
}

// ---------------------------------------------------------------------------
// kill all
// ---------------------------------------------------------------------------

#[test]
fn kill_all_terminates_every_session() {
    let env = Env::new();
    let s1 = env.session("kall-a");
    let s2 = env.session("kall-b");
    let s3 = env.session("kall-c");

    for s in [&s1, &s2, &s3] {
        let (code, _, _) = env.run(&["new", s, "/bin/sh", "-c", "sleep 60"], 5);
        assert_eq!(code, 0);
    }
    for s in [&s1, &s2, &s3] {
        assert!(wait_for(Duration::from_secs(2), || env
            .server_pid(s)
            .is_some()));
    }

    let pids: Vec<u32> = [&s1, &s2, &s3]
        .iter()
        .map(|s| env.server_pid(s).expect("pid"))
        .collect();

    let (code, _, err) = env.run(&["kill", "all"], 10);
    assert_eq!(code, 0, "kill all failed: {err}");

    // All three pids must be dead.
    for &pid in &pids {
        assert!(
            wait_for(Duration::from_secs(2), || unsafe {
                libc::kill(pid as libc::pid_t, 0) != 0
            }),
            "pid {pid} still alive after kill all"
        );
    }

    // No files left.
    for s in [&s1, &s2, &s3] {
        assert!(!env.dir().join(s).exists(), "socket {s} not cleaned");
        assert!(
            !env.dir().join(format!("{s}.lock")).exists(),
            "lock {s} not cleaned"
        );
    }
}

#[test]
fn kill_all_on_empty_dir_succeeds_with_message() {
    let env = Env::new();
    let (code, _, err) = env.run(&["kill", "all"], 5);
    assert_eq!(code, 0, "kill all on empty failed: {err}");
    assert!(
        err.contains("no sessions"),
        "expected 'no sessions' message, got: {err}"
    );
}

#[test]
fn kill_all_sweeps_orphan_lock_files() {
    let env = Env::new();
    let sess = env.session("orphan-sweep");

    // Plant an orphan lock (regular file, no holder, no paired socket).
    let lock = env.dir().join(format!("{sess}.lock"));
    std::fs::write(&lock, b"").unwrap();
    assert!(lock.exists());

    let (code, _, err) = env.run(&["kill", "all"], 5);
    assert_eq!(code, 0, "kill all failed: {err}");

    assert!(!lock.exists(), "orphan lock should have been swept");
}

#[test]
fn kill_all_quiet_suppresses_stderr_when_empty() {
    let env = Env::new();
    let (code, _, err) = env.run(&["kill", "-q", "all"], 5);
    assert_eq!(code, 0);
    assert!(err.is_empty(), "kill -q all should be silent: {err}");
}
