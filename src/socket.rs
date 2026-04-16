use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, CWD, Mode, OFlags};

/// Validate a session name: [a-zA-Z0-9._-], max 64 bytes.
pub fn validate_session_name(name: &str) -> Result<(), io::Error> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name cannot be empty",
        ));
    }
    if name.len() > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name too long (max 64 characters)",
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must contain only alphanumeric characters, dots, underscores, or hyphens",
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must not start with '.' or '-'",
        ));
    }
    Ok(())
}

/// Resolve the socket directory, creating it if needed.
/// Order: MNEME_SOCKET_DIR → XDG_RUNTIME_DIR/mneme → ~/.mneme → TMPDIR/mneme/<uid> → /tmp/mneme/<uid>
pub fn socket_dir() -> Result<PathBuf, io::Error> {
    let uid = rustix::process::getuid().as_raw();

    let candidates: Vec<(PathBuf, bool, bool)> = vec![
        // (path, is_personal, is_explicit) — explicit dirs skip permission checks
        (
            std::env::var("MNEME_SOCKET_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_default(),
            true,
            true, // user explicitly set this — trust it
        ),
        (
            std::env::var("XDG_RUNTIME_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|d| PathBuf::from(d).join("mneme"))
                .unwrap_or_default(),
            true,
            false,
        ),
        // macOS: TMPDIR is per-user and cleaned on reboot — correct
        // place for runtime sockets when XDG_RUNTIME_DIR is unset.
        (
            std::env::var("TMPDIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|d| PathBuf::from(d).join("mneme"))
                .unwrap_or_default(),
            true, // TMPDIR on macOS is already per-user (/var/folders/.../T/)
            false,
        ),
        // Last resort: shared /tmp with per-uid subdirectory
        (PathBuf::from("/tmp/mneme"), false, false),
    ];

    for (base, personal, explicit) in candidates {
        if base.as_os_str().is_empty() {
            continue;
        }

        if personal {
            if explicit {
                // User-provided dir: just ensure it exists and is a directory
                match ensure_dir_exists(&base) {
                    Ok(()) => return Ok(base),
                    Err(_) => continue,
                }
            } else {
                match ensure_safe_dir(&base, uid, Mode::RWXU) {
                    Ok(()) => return Ok(base),
                    Err(_) => continue,
                }
            }
        } else {
            // Shared base + per-uid subdir
            // Create the shared base with sticky bit (anyone can create subdirs)
            match ensure_shared_base(&base) {
                Ok(()) => {}
                Err(_) => continue,
            }
            let user_dir = base.join(uid.to_string());
            match ensure_safe_dir(&user_dir, uid, Mode::RWXU) {
                Ok(()) => return Ok(user_dir),
                Err(_) => continue,
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not find or create socket directory",
    ))
}

/// Build the full socket path for a session name.
pub fn socket_path(name: &str) -> Result<PathBuf, io::Error> {
    let dir = socket_dir()?;
    Ok(dir.join(name))
}

/// Build the lock-file path for a session name.
/// The lock file sits next to the socket and is flocked by the server for
/// the duration of its lifetime. A session is considered stale iff the
/// lock is not held by any process.
pub fn lock_path(name: &str) -> Result<PathBuf, io::Error> {
    let dir = socket_dir()?;
    Ok(dir.join(format!("{name}.lock")))
}

// ---------------------------------------------------------------------------
// Lock-file operations
//
// The server holds an exclusive flock on `<name>.lock` for its entire
// lifetime. The kernel releases the lock when the process dies (clean
// exit, SIGKILL, crash, OOM, power loss), so stale detection is reliable
// without any timeouts or heuristics.
//
// We use flock(2) (BSD-style, whole-file, per-open-file-description).
// It works on macOS and Linux, is cheap, and is released on process
// death. Unlike fcntl locks, flock state doesn't leak across dup()/fork()
// weirdness in the same way.
// ---------------------------------------------------------------------------

/// Try to acquire an exclusive, non-blocking flock on the given path.
/// Creates the file if it does not exist (mode 0600).
///
/// - Ok(Some(fd)) -> lock acquired; caller owns it for as long as they
///   hold the fd. Dropping the fd releases the lock.
/// - Ok(None) -> another process holds the lock (session is live).
/// - Err(e) -> I/O error (permissions, etc).
pub fn try_acquire_lock(path: &Path) -> io::Result<Option<OwnedFd>> {
    let fd = fs::open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;

    // flock is the BSD-style whole-file advisory lock. EX + NB.
    let ret = unsafe {
        libc::flock(
            rustix::fd::AsRawFd::as_raw_fd(&fd),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if ret == 0 {
        Ok(Some(fd))
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

/// Check whether a session is stale (no server holding the lock).
/// Returns true if the session's lock file is absent OR unlockable.
/// If the lock could be acquired, it is immediately released.
pub fn is_session_stale(lock_path: &Path) -> bool {
    // If the lock file doesn't exist, treat as stale (no server to hold it).
    if !lock_path.exists() {
        return true;
    }
    match try_acquire_lock(lock_path) {
        Ok(Some(_fd)) => true, // acquired -> no live server; dropped here
        Ok(None) => false,     // held by someone -> live
        Err(_) => false,       // unknown -> conservative: treat as live
    }
}

/// Clean up a stale session's on-disk state: socket + lock file.
/// Safe to call when `is_session_stale` returned true, because no live
/// process can be using either file.
pub fn cleanup_stale_session(socket_path: &Path, lock_path: &Path) {
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(lock_path);
}

// ---------------------------------------------------------------------------
// Safe directory operations using openat/fstat (no TOCTOU)
// ---------------------------------------------------------------------------

/// Ensure a directory exists (create if needed). No ownership/permission checks.
/// Used for explicitly user-provided directories (MNEME_SOCKET_DIR).
fn ensure_dir_exists(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Verify it's actually a directory
            if path.is_dir() {
                Ok(())
            } else {
                Err(io::Error::other("not a directory"))
            }
        }
        Err(e) => Err(e),
    }
}

/// Ensure a directory exists, is owned by `expected_uid`, and has the
/// given mode. For the leaf directory: uses open(O_DIRECTORY|O_NOFOLLOW)
/// + fstat on the opened fd. Parent path components are allowed to be
///   symlinks (e.g. /var → /private/var on macOS).
fn ensure_safe_dir(path: &Path, expected_uid: u32, mode: Mode) -> io::Result<()> {
    // Resolve symlinks in parent components, keep the leaf unresolved
    let canonical = match path.parent() {
        Some(parent) if parent != Path::new("") => {
            let resolved_parent = parent.canonicalize()?;
            resolved_parent.join(path.file_name().unwrap())
        }
        _ => path.to_path_buf(),
    };

    // Try to create (ignore AlreadyExists)
    match fs::mkdir(&canonical, mode) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }

    // Open the leaf directory with O_NOFOLLOW — refuses symlinks
    let fd = open_dir_nofollow(&canonical)?;

    // Verify ownership and permissions via fstat on the opened fd
    verify_dir_fd(&fd, expected_uid)?;

    Ok(())
}

/// Create a shared base directory (e.g. /tmp/mneme) with sticky bit.
/// We don't verify ownership since it's shared — but we do verify it's
/// a real directory (not a symlink).
fn ensure_shared_base(path: &Path) -> io::Result<()> {
    // Resolve symlinks in parent components
    let canonical = match path.parent() {
        Some(parent) if parent != Path::new("") => {
            let resolved_parent = parent.canonicalize()?;
            resolved_parent.join(path.file_name().unwrap())
        }
        _ => path.to_path_buf(),
    };

    // 1777 = rwxrwxrwx + sticky bit
    let mode = Mode::RWXU | Mode::RWXG | Mode::RWXO | Mode::SVTX;
    match fs::mkdir(&canonical, mode) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }

    let stat = fs::statat(CWD, &canonical, AtFlags::SYMLINK_NOFOLLOW)?;
    let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if file_type != rustix::fs::FileType::Directory {
        return Err(io::Error::other("not a directory"));
    }

    Ok(())
}

/// Open a directory with O_NOFOLLOW | O_DIRECTORY. This refuses to
/// follow symlinks and fails if the path isn't a directory.
fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let fd = fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    Ok(fd)
}

/// Verify a directory fd is owned by the expected uid and has no
/// group/other access (0700).
fn verify_dir_fd(fd: &OwnedFd, expected_uid: u32) -> io::Result<()> {
    let stat = fs::fstat(fd)?;

    // Must be a directory
    let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if file_type != rustix::fs::FileType::Directory {
        return Err(io::Error::other("not a directory"));
    }

    // Must be owned by us
    if stat.st_uid != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory not owned by current user",
        ));
    }

    // Must have no group/other access
    let perms = stat.st_mode & 0o777;
    if perms & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory has insecure permissions",
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_session_names() {
        assert!(validate_session_name("work").is_ok());
        assert!(validate_session_name("my-session").is_ok());
        assert!(validate_session_name("build_123").is_ok());
        assert!(validate_session_name("test.dev").is_ok());
    }

    #[test]
    fn invalid_session_names() {
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("a/b").is_err());
        assert!(validate_session_name("a b").is_err());
        assert!(validate_session_name(".hidden").is_err());
        assert!(validate_session_name("-flag").is_err());
        assert!(validate_session_name(&"a".repeat(65)).is_err());
        assert!(validate_session_name("hello\x00world").is_err());
    }

    #[test]
    fn safe_dir_creation() {
        let tmp = std::env::temp_dir().join("mneme-test-safe-dir");
        let _ = std::fs::remove_dir_all(&tmp);

        let uid = rustix::process::getuid().as_raw();
        ensure_safe_dir(&tmp, uid, Mode::RWXU).unwrap();

        // Verify it exists and has correct permissions
        let fd = open_dir_nofollow(&tmp).unwrap();
        verify_dir_fd(&fd, uid).unwrap();

        // Calling again should succeed (idempotent)
        ensure_safe_dir(&tmp, uid, Mode::RWXU).unwrap();

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_symlink() {
        let tmp = std::env::temp_dir();
        let real_dir = tmp.join("mneme-test-real");
        let sym_link = tmp.join("mneme-test-sym");
        let _ = std::fs::remove_dir_all(&real_dir);
        let _ = std::fs::remove_file(&sym_link);

        std::fs::create_dir_all(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &sym_link).unwrap();

        // open_dir_nofollow should refuse the symlink
        assert!(open_dir_nofollow(&sym_link).is_err());

        let _ = std::fs::remove_dir_all(&real_dir);
        let _ = std::fs::remove_file(&sym_link);
    }
}
