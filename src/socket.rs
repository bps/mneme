use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

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

        let dir = if personal {
            base
        } else {
            // Create shared base dir, then per-uid subdir
            if let Err(_e) = create_dir_mode(&base, 0o1777) {
                continue;
            }
            base.join(uid.to_string())
        };

        match create_dir_mode(&dir, 0o700) {
            Ok(()) => {}
            Err(_) => continue,
        }

        // Verify ownership and permissions
        match verify_dir(&dir, uid) {
            Ok(()) => return Ok(dir),
            Err(_) => continue,
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
// Helpers
// ---------------------------------------------------------------------------

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn create_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn verify_dir(path: &Path, expected_uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::Other, "not a directory"));
    }
    if meta.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory not owned by current user",
        ));
    }
    // Check no group/other access
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory has insecure permissions",
        ));
    }
    Ok(())
}

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
}
