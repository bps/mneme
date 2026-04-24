//! Black-box tests for CLI argument parsers (`parse_detach_key`,
//! `parse_size`). They are private to `main.rs`, so we exercise them
//! through clap's value parsers by passing flags to the `mn` binary.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

fn mn_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mn"))
}

/// Run `mn` with isolated socket dir and return (exit_code, stderr).
fn run(args: &[&str]) -> (i32, String) {
    let dir = TempDir::new().unwrap();
    let mut child = Command::new(mn_bin())
        .env("MNEME_SOCKET_DIR", dir.path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mn");

    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let mut err = String::new();
            use std::io::Read;
            if let Some(mut e) = child.stderr.take() {
                e.read_to_string(&mut err).ok();
            }
            return (status.code().unwrap_or(-1), err);
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mn timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// parse_detach_key
// ---------------------------------------------------------------------------

#[test]
fn detach_key_caret_form_accepted() {
    // ^q (Ctrl-Q) is a perfectly valid detach key. Use a fast-exit child
    // so the process returns quickly without actually attaching to a TTY
    // in a meaningful way.
    let (code, err) = run(&["create", "-e", "^q", "parse-key-1", "/usr/bin/true"]);
    assert_eq!(code, 0, "valid -e ^q rejected: {err}");
}

#[test]
fn detach_key_single_byte_accepted() {
    let (code, err) = run(&["create", "-e", "x", "parse-key-2", "/usr/bin/true"]);
    assert_eq!(code, 0, "valid -e x rejected: {err}");
}

#[test]
fn detach_key_too_long_rejected() {
    let (code, err) = run(&["create", "-e", "abc", "parse-key-3", "/usr/bin/true"]);
    assert_ne!(code, 0, "abc should be rejected as detach key");
    assert!(
        err.contains("invalid detach key"),
        "expected detach-key error, got: {err}"
    );
}

#[test]
fn detach_key_empty_rejected() {
    let (code, err) = run(&["create", "-e", "", "parse-key-4", "/usr/bin/true"]);
    assert_ne!(code, 0, "empty -e should be rejected");
    assert!(
        err.contains("invalid detach key"),
        "expected detach-key error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// parse_size
// ---------------------------------------------------------------------------

#[test]
fn ring_size_plain_integer_accepted() {
    let (code, err) = run(&["create", "-s", "65536", "parse-size-1", "/usr/bin/true"]);
    assert_eq!(code, 0, "65536 rejected: {err}");
}

#[test]
fn ring_size_kilobyte_suffix_accepted() {
    let (code, err) = run(&["create", "-s", "512K", "parse-size-2", "/usr/bin/true"]);
    assert_eq!(code, 0, "512K rejected: {err}");
}

#[test]
fn ring_size_megabyte_suffix_accepted() {
    let (code, err) = run(&["create", "-s", "2M", "parse-size-3", "/usr/bin/true"]);
    assert_eq!(code, 0, "2M rejected: {err}");
}

#[test]
fn ring_size_lowercase_suffix_accepted() {
    let (code, err) = run(&["create", "-s", "4k", "parse-size-4", "/usr/bin/true"]);
    assert_eq!(code, 0, "4k rejected: {err}");
}

#[test]
fn ring_size_garbage_rejected() {
    let (code, err) = run(&["create", "-s", "abc", "parse-size-5", "/usr/bin/true"]);
    assert_ne!(code, 0, "abc should be rejected");
    assert!(
        err.contains("invalid size"),
        "expected size error, got: {err}"
    );
}

#[test]
fn ring_size_negative_rejected() {
    let (code, err) = run(&["create", "-s", "-1", "parse-size-6", "/usr/bin/true"]);
    assert_ne!(code, 0, "-1 should be rejected");
    // Either parse error or clap thinks it's a flag — both are acceptable.
    assert!(
        err.contains("invalid size") || err.contains("unexpected"),
        "expected error for -1, got: {err}"
    );
}
