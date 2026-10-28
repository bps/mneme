//! Direct unit tests for `mneme::socket` helpers — flock acquisition,
//! stale detection, and stale cleanup. These exercise functions that
//! are otherwise only reached transitively through the integration tests
//! and so can have surprising coverage gaps.

use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard, OnceLock};

use mneme::socket;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn env_lock() -> &'static Mutex<()> {
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

/// Each test gets its own MNEME_SOCKET_DIR so socket_dir()/lock_path()
/// resolve into an isolated tempdir. The returned guard must be held
/// for the test's full duration to serialize env mutation across the
/// parallel test runner.
struct DirGuard {
    _td: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl DirGuard {
    fn new() -> Self {
        // SAFETY: env mutation is serialized by holding the global mutex
        // for the entire lifetime of the guard.
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let td = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .expect("tempdir");
        let prev = std::env::var_os("MNEME_SOCKET_DIR");
        unsafe {
            std::env::set_var("MNEME_SOCKET_DIR", td.path());
        }
        Self {
            _td: td,
            prev,
            _lock: lock,
        }
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        // SAFETY: still holding the lock via _lock.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("MNEME_SOCKET_DIR", v),
                None => std::env::remove_var("MNEME_SOCKET_DIR"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// validate_session_name (additional cases beyond src/socket.rs unit tests)
// ---------------------------------------------------------------------------

#[test]
fn validate_session_name_rejects_dotdot() {
    assert!(socket::validate_session_name("..").is_err());
    assert!(socket::validate_session_name("../etc").is_err());
}

// ---------------------------------------------------------------------------
// try_acquire_lock
// ---------------------------------------------------------------------------

#[test]
fn try_acquire_lock_creates_file_and_returns_fd() {
    let _g = DirGuard::new();
    let path = socket::lock_path("acq-1").unwrap();
    assert!(!path.exists());

    let fd = socket::try_acquire_lock(&path).expect("acquire");
    assert!(fd.is_some(), "should acquire fresh lock");
    assert!(path.exists(), "lock file should be created");

    drop(fd);
}

#[test]
fn try_acquire_lock_second_call_returns_none_while_held() {
    let _g = DirGuard::new();
    let path = socket::lock_path("acq-2").unwrap();

    let held = socket::try_acquire_lock(&path)
        .expect("first")
        .expect("some");

    // flock is per-open-file-description: a *new* open from the same
    // process trying LOCK_EX|LOCK_NB will get EWOULDBLOCK. Verify by
    // calling the helper again — it does an independent open.
    let second = socket::try_acquire_lock(&path).expect("second call");
    assert!(second.is_none(), "second acquire should yield None");

    drop(held);
    // After release, a fresh acquire succeeds again.
    let third = socket::try_acquire_lock(&path).expect("third call");
    assert!(
        third.is_some(),
        "lock should be re-acquirable after release"
    );
    drop(third);
}

#[test]
fn try_acquire_lock_drop_releases() {
    let _g = DirGuard::new();
    let path = socket::lock_path("acq-3").unwrap();

    {
        let _held = socket::try_acquire_lock(&path).unwrap().unwrap();
        // Sanity: held; raw fd is positive.
        assert!(_held.as_raw_fd() >= 0);
    } // dropped here

    let after = socket::try_acquire_lock(&path).unwrap();
    assert!(after.is_some(), "drop should release flock");
}

// ---------------------------------------------------------------------------
// is_session_stale
// ---------------------------------------------------------------------------

#[test]
fn is_session_stale_when_lock_file_missing() {
    let _g = DirGuard::new();
    let path = socket::lock_path("stale-missing").unwrap();
    assert!(!path.exists());
    assert!(
        socket::is_session_stale(&path),
        "missing lock file should be stale"
    );
}

#[test]
fn is_session_stale_when_lock_unheld() {
    let _g = DirGuard::new();
    let path = socket::lock_path("stale-unheld").unwrap();
    // Plant an unheld lock file.
    drop(socket::try_acquire_lock(&path).unwrap().unwrap());
    assert!(
        socket::is_session_stale(&path),
        "unheld lock file should be reported stale"
    );
}

#[test]
fn is_session_stale_returns_false_when_held() {
    let _g = DirGuard::new();
    let path = socket::lock_path("stale-held").unwrap();
    let _held = socket::try_acquire_lock(&path).unwrap().unwrap();

    assert!(
        !socket::is_session_stale(&path),
        "held lock should not be reported stale"
    );

    drop(_held);
}

// ---------------------------------------------------------------------------
// cleanup_stale_session
// ---------------------------------------------------------------------------

#[test]
fn cleanup_stale_session_removes_socket_and_preserves_lock() {
    let _g = DirGuard::new();
    let sock = socket::socket_path("cleanup-1").unwrap();
    let lock = socket::lock_path("cleanup-1").unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    drop(socket::try_acquire_lock(&lock).unwrap().unwrap());
    assert!(sock.exists() && lock.exists());

    assert!(socket::cleanup_stale_session(&sock, &lock).unwrap());

    assert!(!sock.exists(), "socket should be gone");
    assert!(lock.exists(), "stable lock must remain");
}

#[test]
fn cleanup_stale_session_is_idempotent() {
    let _g = DirGuard::new();
    let sock = socket::socket_path("cleanup-2").unwrap();
    let lock = socket::lock_path("cleanup-2").unwrap();

    // Files don't exist — must not panic.
    assert!(socket::cleanup_stale_session(&sock, &lock).unwrap());

    // Create just the lock — only one file present is fine.
    drop(socket::try_acquire_lock(&lock).unwrap().unwrap());
    assert!(socket::cleanup_stale_session(&sock, &lock).unwrap());
    assert!(lock.exists());

    // Call again with the same stable inode — still fine.
    assert!(socket::cleanup_stale_session(&sock, &lock).unwrap());
}

// ---------------------------------------------------------------------------
// socket_dir / socket_path / lock_path
// ---------------------------------------------------------------------------

#[test]
fn lock_namespace_is_hidden_and_separate() {
    let _g = DirGuard::new();
    let s = socket::socket_path("share-1").unwrap();
    let l = socket::lock_path("share-1").unwrap();
    assert_eq!(s.parent().unwrap().join(".locks"), l.parent().unwrap());
    assert_eq!(s.file_name().unwrap(), "share-1");
    assert_eq!(l.file_name().unwrap(), "share-1");
}

#[test]
fn socket_dir_honours_env_override() {
    let _g = DirGuard::new();
    let dir = socket::socket_dir().unwrap();
    let env_dir = std::env::var_os("MNEME_SOCKET_DIR").unwrap();
    assert_eq!(dir.as_os_str(), env_dir);
}
