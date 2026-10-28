//! Lock identity, upgrade compatibility, force ordering and filesystem safety.
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

mod common;
use common::private_dir;
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::{Duration, Instant};

use mneme::{protocol, socket};

// fork temporarily inherits CLOEXEC descriptors until exec. Serialize this
// binary's tests so subprocess launches cannot prolong another test's lock.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock(dir: &Path, name: &str) -> std::path::PathBuf {
    socket::lock_path_for_socket(&dir.join(name)).unwrap()
}

fn cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mn"));
    cmd.env("MNEME_SOCKET_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn wait(child: &mut Child) -> Output {
    common::collect_output(child, Duration::from_secs(8))
}

fn until(predicate: impl FnMut() -> bool) {
    assert!(
        common::wait_for(Duration::from_secs(5), predicate),
        "condition deadline exceeded"
    );
}

use common::query;

#[test]
fn stale_probe_does_not_authorize_later_cleanup() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let path = dir.path().join("race");
    let lock = lock(dir.path(), "race");
    let _listener = UnixListener::bind(&path).unwrap();
    assert!(socket::is_session_stale(&lock));
    let identity = std::fs::metadata(&lock).unwrap().ino();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let other_lock = lock.clone();
    let owner = std::thread::spawn(move || {
        let _held = socket::try_acquire_lock(&other_lock).unwrap().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!socket::cleanup_stale_session(&path, &lock).unwrap());
    assert!(path.exists());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), identity);
    release_tx.send(()).unwrap();
    owner.join().unwrap();
    assert!(socket::cleanup_stale_session(&path, &lock).unwrap());
    assert!(!path.exists());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), identity);
}

#[test]
fn concurrent_lockers_share_one_inode_and_one_winner() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let lock = lock(dir.path(), "concurrent");
    drop(socket::try_acquire_lock(&lock).unwrap().unwrap());
    let start = Arc::new(Barrier::new(9));
    let release = Arc::new(Barrier::new(9));
    let (tx, rx) = mpsc::channel();
    let mut threads = Vec::new();
    for _ in 0..8 {
        let (start, release, tx, lock) = (start.clone(), release.clone(), tx.clone(), lock.clone());
        threads.push(std::thread::spawn(move || {
            start.wait();
            let held = socket::try_acquire_lock(&lock).unwrap();
            tx.send(held.is_some()).unwrap();
            release.wait();
            drop(held);
        }));
    }
    start.wait();
    let winners = (0..8)
        .filter(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .count();
    release.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(winners, 1);
}

#[test]
fn unsafe_lock_inodes_are_rejected_without_removal_or_fifo_hang() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let target = dir.path().join("target");
    std::fs::write(&target, b"preserve").unwrap();
    let sym = lock(dir.path(), "symlink");
    symlink(&target, &sym).unwrap();
    assert!(socket::try_acquire_lock(&sym).is_err());
    assert!(sym.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve");

    let hard = lock(dir.path(), "hardlink");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&target, &hard).unwrap();
    assert!(socket::try_acquire_lock(&hard).is_err());
    assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 2);

    let fifo = lock(dir.path(), "fifo");
    use std::os::unix::ffi::OsStrExt;
    let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        tx.send(socket::try_acquire_lock(&fifo).is_err()).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
    handle.join().unwrap();

    let directory = lock(dir.path(), "directory");
    std::fs::create_dir(&directory).unwrap();
    assert!(socket::try_acquire_lock(&directory).is_err());

    let public = lock(dir.path(), "public");
    std::fs::write(&public, []).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(socket::try_acquire_lock(&public).is_err());
}

#[test]
fn stale_cleanup_preserves_regular_files_and_symlinks() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let path = dir.path().join("file");
    let lock = lock(dir.path(), "file");
    std::fs::write(&path, b"preserve").unwrap();
    assert!(socket::cleanup_stale_session(&path, &lock).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
    let sym = dir.path().join("sym");
    symlink(&path, &sym).unwrap();
    assert!(socket::cleanup_stale_session(&sym, &self::lock(dir.path(), "sym")).is_err());
    assert!(sym.symlink_metadata().unwrap().file_type().is_symlink());
}

#[test]
fn active_legacy_lock_blocks_creation_and_cleanup() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let path = dir.path().join("upgrade");
    let _listener = UnixListener::bind(&path).unwrap();
    let legacy = dir.path().join("upgrade.lock");
    let _legacy_holder = socket::try_acquire_lock(&legacy).unwrap().unwrap();
    let lock = lock(dir.path(), "upgrade");
    let ino = path.symlink_metadata().unwrap().ino();
    assert!(socket::try_acquire_lock(&lock).unwrap().is_none());
    assert!(!socket::cleanup_stale_session(&path, &lock).unwrap());
    let result = wait(
        &mut cmd(dir.path())
            .args(["new", "upgrade", "/usr/bin/true"])
            .spawn()
            .unwrap(),
    );
    assert!(!result.status.success());
    assert_eq!(path.symlink_metadata().unwrap().ino(), ino);
    assert!(legacy.exists());
}

#[test]
fn explicit_runtime_and_hidden_directory_checks_fail_closed() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    for mode in [0o755, 0o777] {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let result = wait(&mut cmd(dir.path()).arg("list").spawn().unwrap());
        assert!(!result.status.success(), "mode {mode:o} was accepted");
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let parent = private_dir();
    let sym = parent.path().join("runtime");
    symlink(dir.path(), &sym).unwrap();
    let result = wait(&mut cmd(&sym).arg("list").spawn().unwrap());
    assert!(!result.status.success());

    // A symlink in a parent component, unlike the runtime leaf, is valid.
    let nested = dir.path().join("private");
    std::fs::create_dir(&nested).unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(socket::lock_path_for_socket(&sym.join("private/session")).is_ok());
    let hidden = nested.join(".locks");
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(socket::lock_path_for_socket(&nested.join("session")).is_err());
}

#[test]
fn socket_cleanup_does_not_remove_replacement_inode() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let path = dir.path().join("identity");
    let _listener = UnixListener::bind(&path).unwrap();
    let guard = socket::SocketCleanup::new(&path).unwrap();
    std::fs::rename(&path, dir.path().join("old")).unwrap();
    let _replacement = UnixListener::bind(&path).unwrap();
    let ino = path.symlink_metadata().unwrap().ino();
    drop(guard);
    assert_eq!(path.symlink_metadata().unwrap().ino(), ino);
}

#[test]
fn names_with_lock_suffix_are_discoverable_and_independent() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    for name in ["work", "work.lock"] {
        let out = wait(
            &mut cmd(dir.path())
                .args(["new", name, "/usr/bin/true"])
                .spawn()
                .unwrap(),
        );
        assert!(out.status.success(), "{out:?}");
        until(|| query(&dir.path().join(name)).is_ok());
    }
    let out = wait(&mut cmd(dir.path()).arg("list").spawn().unwrap());
    let listing = String::from_utf8(out.stdout).unwrap();
    assert!(listing.lines().any(|l| l.ends_with("  work")));
    assert!(listing.lines().any(|l| l.ends_with("  work.lock")));
    let out = wait(&mut cmd(dir.path()).args(["kill", "work"]).spawn().unwrap());
    assert!(out.status.success(), "{out:?}");
    assert!(query(&dir.path().join("work.lock")).is_ok());
    assert!(
        wait(&mut cmd(dir.path()).args(["kill", "work.lock"]).spawn().unwrap())
            .status
            .success()
    );
}

#[test]
fn concurrent_creators_start_only_one_child() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let marker = dir.path().join("starts");
    let barrier = Arc::new(Barrier::new(9));
    let (finished_tx, finished_rx) = mpsc::channel();
    for _ in 0..8 {
        let barrier = barrier.clone();
        let dir = dir.path().to_owned();
        let command = format!("echo started >> '{}'", marker.display());
        let finished_tx = finished_tx.clone();
        std::thread::spawn(move || {
            barrier.wait();
            let output = wait(
                &mut cmd(&dir)
                    .args(["new", "race", "/bin/sh", "-c", &command])
                    .spawn()
                    .unwrap(),
            );
            let _ = finished_tx.send(output);
        });
    }
    drop(finished_tx);
    barrier.wait();
    // Bound the caller too: spawn or a broken worker must not leave the
    // suite blocked in JoinHandle::join while holding TEST_LOCK.
    let deadline = Instant::now() + Duration::from_secs(12);
    let successes = (0..8)
        .map(|_| {
            finished_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("concurrent creator did not report before the collection deadline")
        })
        .filter(|output| output.status.success())
        .count();
    assert_eq!(successes, 1, "only the winning server may report readiness");
    until(|| marker.exists() && query(&dir.path().join("race")).is_ok());
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert!(
        wait(&mut cmd(dir.path()).args(["kill", "race"]).spawn().unwrap())
            .status
            .success()
    );
}

#[test]
fn force_refuses_unresponsive_lock_owner_without_unlinking() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let path = dir.path().join("stuck");
    let _listener = UnixListener::bind(&path).unwrap();
    let lock = lock(dir.path(), "stuck");
    let _holder = socket::try_acquire_lock(&lock).unwrap().unwrap();
    let ino = path.symlink_metadata().unwrap().ino();
    let out = wait(
        &mut cmd(dir.path())
            .args(["new", "-f", "stuck", "/usr/bin/true"])
            .spawn()
            .unwrap(),
    );
    assert!(!out.status.success());
    assert_eq!(path.symlink_metadata().unwrap().ino(), ino);
    assert!(socket::try_acquire_lock(&lock).unwrap().is_none());
}

// This subprocess simulates an old server with a deliberately delayed SIGTERM
// cleanup. The parent controls lock release through a file, not a timing sleep.
static TERM_RECEIVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn record_term(_: libc::c_int) {
    TERM_RECEIVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn force_peer_helper() {
    let Some(dir) = std::env::var_os("MNEME_TEST_FORCE_HELPER") else {
        return;
    };
    let dir = Path::new(&dir);
    unsafe {
        libc::signal(
            libc::SIGTERM,
            record_term as *const () as libc::sighandler_t,
        );
    }
    let lock_path = if std::env::var_os("MNEME_TEST_LEGACY").is_some() {
        dir.join("force.lock")
    } else {
        lock(dir, "force")
    };
    let _held = socket::try_acquire_lock(&lock_path).unwrap().unwrap();
    let listener = UnixListener::bind(dir.join("force")).unwrap();
    std::fs::write(dir.join("ready"), []).unwrap();
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let hello = protocol::recv_packet(stream.as_fd()).unwrap();
    assert_eq!(hello.parse_hello().unwrap().intent, protocol::Intent::Query);
    protocol::send_packet(
        stream.as_fd(),
        &protocol::Packet::welcome(&protocol::Welcome {
            version: protocol::PROTOCOL_VERSION,
            server_pid: std::process::id(),
            child_pid: 0,
            child_running: true,
            exit_status: 0,
            client_count: 0,
            ring_size: 1024,
            ring_used: 0,
        }),
    )
    .unwrap();
    until(|| TERM_RECEIVED.load(std::sync::atomic::Ordering::Relaxed));
    std::fs::write(dir.join("terminated"), []).unwrap();
    until(|| dir.join("release").exists());
    socket::remove_socket(&dir.join("force")).unwrap();
}

#[test]
fn force_waits_for_lock_release_before_starting_replacement() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for legacy in [false, true] {
        let dir = private_dir();
        let mut helper_cmd = Command::new(std::env::current_exe().unwrap());
        helper_cmd
            .args(["--exact", "force_peer_helper", "--nocapture"])
            .env("MNEME_TEST_FORCE_HELPER", dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if legacy {
            helper_cmd.env("MNEME_TEST_LEGACY", "1");
        }
        let mut helper = helper_cmd.spawn().unwrap();
        until(|| dir.path().join("ready").exists());
        let lock = lock(dir.path(), "force");
        // Creates the new inode even when the legacy holder blocks acquisition.
        assert!(socket::try_acquire_lock(&lock).unwrap().is_none());
        let ino = std::fs::metadata(&lock).unwrap().ino();
        let marker = dir.path().join("replacement");
        let command = format!("echo started > '{}'", marker.display());
        let mut force = cmd(dir.path())
            .args(["new", "-f", "force", "/bin/sh", "-c", &command])
            .spawn()
            .unwrap();
        until(|| dir.path().join("terminated").exists());
        assert!(force.try_wait().unwrap().is_none());
        assert!(!marker.exists());
        assert!(socket::try_acquire_lock(&lock).unwrap().is_none());
        std::fs::write(dir.path().join("release"), []).unwrap();
        assert!(wait(&mut helper).status.success());
        let out = wait(&mut force);
        assert!(out.status.success(), "{out:?}");
        until(|| marker.exists() && query(&dir.path().join("force")).is_ok());
        assert_eq!(std::fs::metadata(&lock).unwrap().ino(), ino);
        assert!(
            wait(&mut cmd(dir.path()).args(["kill", "force"]).spawn().unwrap())
                .status
                .success()
        );
    }
}

#[test]
fn probes_coexist_and_retain_legacy_guards() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let legacy = dir.path().join("probe.lock");
    drop(socket::try_acquire_lock(&legacy).unwrap().unwrap());
    let lock = lock(dir.path(), "probe");
    let probe1 = socket::try_probe_lock(&lock).unwrap().unwrap();
    let probe2 = socket::try_probe_lock(&lock).unwrap().unwrap();
    assert!(socket::is_session_stale(&lock));
    assert!(socket::try_acquire_lock(&lock).unwrap().is_none());
    assert!(socket::try_acquire_lock(&legacy).unwrap().is_none());
    assert!(socket::cleanup_stale_session(&dir.path().join("probe"), &lock).unwrap());
    drop(probe1);
    drop(probe2);
    assert!(socket::try_acquire_lock(&legacy).unwrap().is_some());
}

#[test]
fn force_timeout_preserves_live_socket_and_lock() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "force_peer_helper", "--nocapture"])
        .env("MNEME_TEST_FORCE_HELPER", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until(|| dir.path().join("ready").exists());
    let socket_path = dir.path().join("force");
    let ino = socket_path.symlink_metadata().unwrap().ino();
    let out = wait(
        &mut cmd(dir.path())
            .args(["new", "-f", "force", "/usr/bin/true"])
            .spawn()
            .unwrap(),
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not exit"));
    assert_eq!(socket_path.symlink_metadata().unwrap().ino(), ino);
    assert!(
        socket::try_acquire_lock(&lock(dir.path(), "force"))
            .unwrap()
            .is_none()
    );
    std::fs::write(dir.path().join("release"), []).unwrap();
    assert!(wait(&mut helper).status.success());
}

#[test]
fn restricted_umask_helper() {
    let Some(dir) = std::env::var_os("MNEME_TEST_UMASK_HELPER") else {
        return;
    };
    unsafe {
        libc::umask(0o277);
    }
    let lock = lock(Path::new(&dir), "umask");
    let _held = socket::try_acquire_lock(&lock).unwrap().unwrap();
    assert_eq!(std::fs::metadata(lock).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn restrictive_umask_does_not_poison_new_lock_inodes() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "restricted_umask_helper", "--nocapture"])
        .env("MNEME_TEST_UMASK_HELPER", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait(&mut helper);
    assert!(out.status.success(), "{out:?}");
    assert!(
        socket::try_acquire_lock(&lock(dir.path(), "umask"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn server_waits_for_discovery_probe_to_release() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    let lock = lock(dir.path(), "probe-start");
    let probe = socket::try_probe_lock(&lock).unwrap().unwrap();
    let log = dir.path().join("server.log");
    let marker = dir.path().join("started");
    let command = format!("echo started > '{}'", marker.display());
    let mut creator = cmd(dir.path())
        .env("MNEME_SERVER_LOG", &log)
        .args(["new", "probe-start", "/bin/sh", "-c", &command])
        .spawn()
        .unwrap();
    until(|| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("startup")
    });
    assert!(!marker.exists(), "server bypassed the probe's shared lock");
    assert!(creator.try_wait().unwrap().is_none());
    drop(probe);
    let out = wait(&mut creator);
    assert!(out.status.success(), "{out:?}");
    until(|| marker.exists() && query(&dir.path().join("probe-start")).is_ok());
    assert!(
        wait(
            &mut cmd(dir.path())
                .args(["kill", "probe-start"])
                .spawn()
                .unwrap()
        )
        .status
        .success()
    );
}

#[test]
// Deliberately exit before this descendant to test inherited pipe writers.
#[allow(clippy::zombie_processes)]
fn capture_direct_helper() {
    let Some(dir) = std::env::var_os("MNEME_TEST_CAPTURE_HELPER") else {
        return;
    };
    let _holder = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "capture_holder_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MNEME_TEST_CAPTURE_HOLDER", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    println!("capture-direct-stdout");
    eprintln!("capture-direct-stderr");
}

#[test]
fn capture_holder_helper() {
    let Some(dir) = std::env::var_os("MNEME_TEST_CAPTURE_HOLDER") else {
        return;
    };
    let dir = Path::new(&dir);
    std::fs::write(dir.join("holder-ready"), []).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.join("holder-release").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(dir.join("holder-exited"), []).unwrap();
    assert!(
        dir.join("holder-release").exists(),
        "holder release deadline exceeded"
    );
}

#[test]
fn collection_returns_after_child_exit_without_inherited_pipe_eof() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = private_dir();
    // Always release the descendant if an assertion fails.
    struct ReleaseOnDrop(std::path::PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, []);
        }
    }
    let release = ReleaseOnDrop(dir.path().join("holder-release"));
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "capture_direct_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MNEME_TEST_CAPTURE_HELPER", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until(|| dir.path().join("holder-ready").exists());
    let out = wait(&mut helper);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("capture-direct-stdout"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("capture-direct-stderr"));
    assert!(
        !dir.path().join("holder-exited").exists(),
        "collector waited for inherited pipe EOF"
    );
    drop(release);
    until(|| dir.path().join("holder-exited").exists());
}

#[test]
fn capture_large_output_helper() {
    if std::env::var_os("MNEME_TEST_LARGE_CAPTURE").is_none() {
        return;
    }
    use std::io::Write;
    std::io::stdout()
        .write_all(&vec![b'x'; 256 * 1024])
        .unwrap();
    std::io::stderr()
        .write_all(&vec![b'y'; 256 * 1024])
        .unwrap();
}

#[test]
fn collection_drains_both_pipes_before_child_exit() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "capture_large_output_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MNEME_TEST_LARGE_CAPTURE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait(&mut helper);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out.stdout.iter().filter(|&&b| b == b'x').count(),
        256 * 1024
    );
    assert_eq!(
        out.stderr.iter().filter(|&&b| b == b'y').count(),
        256 * 1024
    );
}
