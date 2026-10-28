use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};

/// Validate a session name: [a-zA-Z0-9._-], max 64 bytes.
pub fn validate_session_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with(['.', '-'])
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must be 1–64 alphanumeric, dot, underscore or hyphen characters and must not start with dot or hyphen",
        ));
    }
    Ok(())
}

/// Resolve a private runtime directory. Explicit overrides fail closed.
/// Parent symlinks (notably /var on macOS) are allowed; leaf symlinks are not.
pub fn socket_dir() -> io::Result<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    if let Some(dir) = std::env::var_os("MNEME_SOCKET_DIR").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(dir);
        ensure_safe_dir(&path, uid, Mode::RWXU)?;
        return Ok(path);
    }
    for key in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(base) = std::env::var_os(key).filter(|s| !s.is_empty()) {
            let path = PathBuf::from(base).join("mneme");
            if ensure_safe_dir(&path, uid, Mode::RWXU).is_ok() {
                return Ok(path);
            }
        }
    }
    let base = Path::new("/tmp/mneme");
    ensure_shared_base(base)?;
    let path = base.join(uid.to_string());
    ensure_safe_dir(&path, uid, Mode::RWXU)?;
    Ok(path)
}

pub fn socket_path(name: &str) -> io::Result<PathBuf> {
    validate_session_name(name)?;
    Ok(socket_dir()?.join(name))
}

/// Lock inodes are permanent and live in a namespace no valid name can use.
pub fn lock_path(name: &str) -> io::Result<PathBuf> {
    lock_path_for_socket(&socket_path(name)?)
}

pub fn lock_path_for_socket(socket: &Path) -> io::Result<PathBuf> {
    let name = socket
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::other("invalid socket path"))?;
    validate_session_name(name)?;
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::other("missing socket directory"))?;
    let uid = rustix::process::geteuid().as_raw();
    ensure_safe_dir(parent, uid, Mode::RWXU)?;
    let dir = parent.join(".locks");
    ensure_safe_dir(&dir, uid, Mode::RWXU)?;
    Ok(dir.join(name))
}

fn open_lock(path: &Path, create: bool) -> io::Result<OwnedFd> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing lock directory"))?;
    let dir = open_dir_nofollow(parent)?;
    verify_dir_fd(&dir, rustix::process::geteuid().as_raw())?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing lock name"))?;
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    // EXCL distinguishes new files so restrictive umasks can be corrected
    // without changing permissions of an existing inode.
    let fd = if create {
        match fs::openat(
            &dir,
            name,
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => {
                fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
                fd
            }
            Err(rustix::io::Errno::EXIST) => fs::openat(&dir, name, flags, Mode::empty())?,
            Err(e) => return Err(e.into()),
        }
    } else {
        fs::openat(&dir, name, flags, Mode::empty())?
    };
    let stat = fs::fstat(&fd)?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe session lock (expected private, owned regular file with one link)",
        ));
    }
    Ok(fd)
}

fn flock(fd: &OwnedFd, exclusive: bool) -> io::Result<bool> {
    let kind = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(fd.as_raw_fd(), kind | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Both locks, when present, stay held until the operation finishes.
pub struct SessionLock {
    current: OwnedFd,
    _legacy: Option<OwnedFd>,
}

impl AsRawFd for SessionLock {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.current.as_raw_fd()
    }
}

/// Acquire a stable inode exclusively for a server. Never unlink it.
/// Existing legacy inodes are also held, but never created or removed:
/// creating them would collide with valid names ending in ".lock".
pub fn try_acquire_lock(path: &Path) -> io::Result<Option<SessionLock>> {
    acquire_lock(path, true)
}

/// Probes and cleaners can coexist. A shared guard excludes every server.
pub fn try_probe_lock(path: &Path) -> io::Result<Option<SessionLock>> {
    acquire_lock(path, false)
}

fn acquire_lock(path: &Path, exclusive: bool) -> io::Result<Option<SessionLock>> {
    let fd = open_lock(path, true)?;
    if !flock(&fd, exclusive)? {
        return Ok(None);
    }
    let mut legacy_fd = None;
    if path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".locks")) {
        let legacy = path.parent().unwrap().parent().unwrap().join(format!(
            "{}.lock",
            path.file_name().unwrap().to_string_lossy()
        ));
        match std::fs::symlink_metadata(&legacy) {
            // This can be another valid session, not a pre-upgrade lock.
            Ok(meta) if meta.file_type().is_socket() => {}
            Ok(_) => {
                let old = open_lock(&legacy, false).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "unsafe legacy lock {}: {e}; inspect it manually",
                            legacy.display()
                        ),
                    )
                })?;
                if !flock(&old, exclusive)? {
                    return Ok(None);
                }
                legacy_fd = Some(old);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(SessionLock {
        current: fd,
        _legacy: legacy_fd,
    }))
}

/// A diagnostic only. Destructive operations must acquire their own guard.
pub fn is_session_stale(path: &Path) -> bool {
    matches!(try_probe_lock(path), Ok(Some(_)))
}

/// Remove only an owned Unix socket. Callers must hold the session lock.
pub fn remove_socket(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_socket()
                && meta.uid() == rustix::process::geteuid().as_raw() =>
        {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to remove a non-socket or foreign socket",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Recheck and retain the lock throughout cleanup, not just during a probe.
/// Returns false if a server owns either the current or a legacy lock.
pub fn cleanup_stale_session(socket: &Path, lock: &Path) -> io::Result<bool> {
    let Some(_guard) = try_probe_lock(lock)? else {
        return Ok(false);
    };
    remove_socket(socket)?;
    Ok(true)
}

/// Remove this server's socket on all normal/error unwinds, before its lock
/// is released. Identity checking also protects against same-UID replacement.
pub struct SocketCleanup {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketCleanup {
    pub fn new(path: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.to_owned(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = remove_socket(&self.path);
        }
    }
}

/// Authenticate the connected peer before sending any protocol data.
pub fn verify_peer_uid(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    verify_peer_uid_for(stream, rustix::process::geteuid().as_raw())
}

fn verify_peer_uid_for(
    stream: &std::os::unix::net::UnixStream,
    expected_uid: u32,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let uid = rustix::net::sockopt::socket_peercred(stream.as_fd())?
        .uid
        .as_raw();
    #[cfg(target_os = "macos")]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credentials unavailable",
    ));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if uid != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket peer belongs to another user",
        ));
    }
    Ok(())
}

fn checked_parent(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize()?;
    // A writable non-sticky ancestor permits replacing the private directory.
    // Sticky ancestors are safe only if their owner is trusted.
    let uid = rustix::process::geteuid().as_raw();
    for ancestor in parent.ancestors() {
        let meta = std::fs::metadata(ancestor)?;
        if (meta.uid() != 0 && meta.uid() != uid)
            || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe runtime directory ancestor",
            ));
        }
    }
    Ok(parent.join(
        path.file_name()
            .ok_or_else(|| io::Error::other("missing directory name"))?,
    ))
}

fn ensure_safe_dir(path: &Path, expected_uid: u32, mode: Mode) -> io::Result<()> {
    let path = checked_parent(path)?;
    match fs::mkdir(&path, mode) {
        Ok(()) => {
            let fd = open_dir_nofollow(&path)?;
            fs::fchmod(&fd, mode)?;
        }
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    verify_dir_fd(&open_dir_nofollow(&path)?, expected_uid)
}

fn ensure_shared_base(path: &Path) -> io::Result<()> {
    let path = checked_parent(path)?;
    let mode = Mode::RWXU | Mode::RWXG | Mode::RWXO | Mode::SVTX;
    match fs::mkdir(&path, mode) {
        Ok(()) => fs::fchmod(open_dir_nofollow(&path)?, mode)?,
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    let stat = fs::fstat(open_dir_nofollow(&path)?)?;
    let uid = rustix::process::geteuid().as_raw();
    if (stat.st_uid != 0 && stat.st_uid != uid) || stat.st_mode & 0o1000 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe shared runtime directory",
        ));
    }
    Ok(())
}

fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    Ok(fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

fn verify_dir_fd(fd: &OwnedFd, expected_uid: u32) -> io::Result<()> {
    let stat = fs::fstat(fd)?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::Directory
        || stat.st_uid != expected_uid
        || stat.st_mode & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory must be owned by current user with mode 0700",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticates_real_peer_credentials() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        verify_peer_uid(&stream).unwrap();
        let other_uid = rustix::process::geteuid().as_raw().wrapping_add(1);
        assert_eq!(
            verify_peer_uid_for(&stream, other_uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

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
