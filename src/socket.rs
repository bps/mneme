use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, Mode, OFlags, CWD};

/// Validate a session name: [a-zA-Z0-9._-], max 64 bytes.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("session name too long (max 64 characters)".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(
            "session name must contain only alphanumeric characters, dots, underscores, or hyphens"
                .into(),
        );
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err("session name must not start with '.' or '-'".into());
    }
    Ok(())
}

/// Resolve the socket directory, creating it if needed.
/// Order: MNEME_SOCKET_DIR → XDG_RUNTIME_DIR/mneme → ~/.mneme → TMPDIR/mneme/<uid> → /tmp/mneme/<uid>
pub fn socket_dir() -> Result<PathBuf, io::Error> {
    let uid = rustix::process::getuid().as_raw();

    let candidates: Vec<(PathBuf, bool)> = vec![
        // (path, is_personal) — personal dirs are 0700, shared need uid subdir
        (
            std::env::var("MNEME_SOCKET_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_default(),
            true,
        ),
        (
            std::env::var("XDG_RUNTIME_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|d| PathBuf::from(d).join("mneme"))
                .unwrap_or_default(),
            true, // XDG_RUNTIME_DIR is already per-user
        ),
        (
            dirs_home().map(|d| d.join(".mneme")).unwrap_or_default(),
            true,
        ),
        (
            std::env::var("TMPDIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|d| PathBuf::from(d).join("mneme"))
                .unwrap_or_default(),
            false,
        ),
        (PathBuf::from("/tmp/mneme"), false),
    ];

    for (base, personal) in candidates {
        if base.as_os_str().is_empty() {
            continue;
        }

        if personal {
            // Single directory: create and verify in one shot
            match ensure_safe_dir(&base, uid, Mode::RWXU) {
                Ok(()) => return Ok(base),
                Err(_) => continue,
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

// ---------------------------------------------------------------------------
// Safe directory operations using openat/fstat (no TOCTOU)
// ---------------------------------------------------------------------------

/// Ensure a directory exists, is owned by `expected_uid`, and has the
/// given mode. For the leaf directory: uses open(O_DIRECTORY|O_NOFOLLOW)
/// + fstat on the opened fd. Parent path components are allowed to be
/// symlinks (e.g. /var → /private/var on macOS).
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
        return Err(io::Error::new(io::ErrorKind::Other, "not a directory"));
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
        return Err(io::Error::new(io::ErrorKind::Other, "not a directory"));
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

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
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
