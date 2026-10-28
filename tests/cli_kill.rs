//! Integration tests for `mn kill <name>` and `mn kill all`.
//!
//! These exercise the live-server kill path (query → SIGTERM → wait for
//! lock release), stale-socket cleanup, preservation of permanent locks,
//! and the not-found error branch.

mod common;

use common::{Env, wait_for};
use std::time::Duration;

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

    // The socket is cleaned up; the stable lock remains.
    let sock = env.dir().join(&sess);
    let lock = env.dir().join(".locks").join(&sess);
    assert!(!sock.exists(), "socket not cleaned");
    assert!(lock.exists(), "stable lock inode must remain");
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
    let lock = env.dir().join(".locks").join(&sess);
    assert!(sock.exists() || lock.exists(), "expected stale files");

    // kill should detect the stale session and clean it up.
    let (code, _, err) = env.run(&["kill", &sess], 5);
    assert_eq!(code, 0, "kill of stale failed: {err}");
    assert!(
        err.contains("stale") || err.contains("killed"),
        "expected stale/killed message, got: {err}"
    );
    assert!(!sock.exists(), "socket not cleaned");
    assert!(lock.exists(), "stable lock inode must remain");
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

    // No sockets left; lock inodes remain stable.
    for s in [&s1, &s2, &s3] {
        assert!(!env.dir().join(s).exists(), "socket {s} not cleaned");
        assert!(
            env.dir().join(".locks").join(s).exists(),
            "stable lock {s} must remain"
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
fn kill_all_preserves_orphan_lock_files() {
    let env = Env::new();
    let sess = env.session("orphan-sweep");

    // Plant a legacy orphan file. Never unlink arbitrary regular files.
    let lock = env.dir().join(format!("{sess}.lock"));
    std::fs::write(&lock, b"").unwrap();
    assert!(lock.exists());

    let (code, _, err) = env.run(&["kill", "all"], 5);
    assert_eq!(code, 0, "kill all failed: {err}");

    assert!(lock.exists(), "orphan regular files must be preserved");
}

#[test]
fn kill_all_quiet_suppresses_stderr_when_empty() {
    let env = Env::new();
    let (code, _, err) = env.run(&["kill", "-q", "all"], 5);
    assert_eq!(code, 0);
    assert!(err.is_empty(), "kill -q all should be silent: {err}");
}
