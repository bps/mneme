//! Shared, bounded integration-test harnesses. No production behavior lives here.
#![allow(dead_code)]

use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use mneme::{protocol, socket};
use protocol::Packet;

/// Use the production codec, which now honors partial-read socket timeouts.
/// Callers must discard the connection after any partial-frame error.
pub fn recv_packet(stream: &UnixStream) -> io::Result<Packet> {
    protocol::recv_packet(stream.as_fd())
}

pub fn private_dir() -> tempfile::TempDir {
    // macOS's default TMPDIR can exceed sockaddr_un's path budget.
    tempfile::Builder::new()
        .prefix("mn-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .expect("short private runtime directory")
}

pub fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

/// Send a raw Hello with bounded reads and writes. The caller owns packet assertions.
pub fn hello(path: &Path, hello: &protocol::Hello) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    socket::verify_peer_uid(&stream)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    protocol::send_packet(stream.as_fd(), &protocol::Packet::hello(hello))?;
    Ok(stream)
}

pub fn query(path: &Path) -> io::Result<protocol::Welcome> {
    let stream = hello(
        path,
        &protocol::Hello {
            version: protocol::PROTOCOL_VERSION,
            intent: protocol::Intent::Query,
            flags: protocol::ClientFlags::empty(),
            rows: 0,
            cols: 0,
        },
    )?;
    recv_packet(&stream)?
        .parse_welcome()
        .ok_or_else(|| io::Error::other("expected Welcome"))
}

/// Observe registration/detach processing instead of assuming a scheduling delay.
pub fn wait_for_client_count(path: &Path, expected: u16) {
    assert!(
        wait_for(Duration::from_secs(5), || {
            query(path).is_ok_and(|welcome| welcome.client_count == expected)
        }),
        "client count did not become {expected}: {}",
        path.display()
    );
}

/// Give direct children bounded kill/reap cleanup even if a test assertion unwinds.
pub struct ChildGuard(pub Child);

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

/// Poll for reaping without waiting indefinitely for a kernel-exiting child.
pub fn reap_child(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child reap deadline exceeded",
            ));
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

struct ReapOnDrop<'a>(&'a mut Child);
impl Drop for ReapOnDrop<'_> {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let kill_error = self.0.kill().err();
            if let Err(error) = reap_child(self.0, Duration::from_secs(1)) {
                // Preserve evidence without panicking during an existing unwind.
                // Do not block on wait() or signal unrelated processes/groups.
                use std::io::Write;
                let _ = writeln!(
                    io::stderr(),
                    "test cleanup: child {} not reaped: {error}; kill error: {kill_error:?}",
                    self.0.id()
                );
            }
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(ReapOnDrop(&mut self.0));
    }
}

// Drain both pipes while the direct child runs. A descendant can retain a
// writer after the child exits, so EOF is not a completion condition.
fn read_available(
    reader: &mut impl io::Read,
    output: &mut Vec<u8>,
    deadline: Instant,
) -> io::Result<()> {
    let mut buffer = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "output collection deadline exceeded",
            ));
        }
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

pub fn collect_output(child: &mut Child, timeout: Duration) -> Output {
    let _cleanup = ReapOnDrop(child);
    let child = &mut *_cleanup.0;
    let deadline = Instant::now() + timeout;
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    for fd in [stdout_pipe.as_fd(), stderr_pipe.as_fd()] {
        let flags = rustix::fs::fcntl_getfl(fd).unwrap();
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let result = (|| {
            read_available(&mut stdout_pipe, &mut stdout, deadline)?;
            read_available(&mut stderr_pipe, &mut stderr, deadline)?;
            let status = child.try_wait()?;
            if status.is_some() {
                // All writes from the direct child are now buffered. Collect
                // them, but do not wait for EOF from unrelated inherited copies.
                read_available(&mut stdout_pipe, &mut stdout, deadline)?;
                read_available(&mut stderr_pipe, &mut stderr, deadline)?;
            }
            Ok::<_, io::Error>(status)
        })();
        match result {
            Ok(Some(status)) => {
                return Output {
                    status,
                    stdout,
                    stderr,
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                // ReapOnDrop performs bounded kill/reap cleanup while unwinding.
                panic!("subprocess collection failed: {e}");
            }
        }
    }
}

/// Reject special kill(2) targets and values that would become negative groups.
pub fn checked_server_pid(pid: u32) -> Option<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    (pid > 1 && pid != std::process::id() as libc::pid_t).then_some(pid)
}

/// Own the directory until its servers have shut down, including during unwind.
/// Construct this before client guards so clients are dropped first.
pub struct RuntimeDir(tempfile::TempDir);

impl RuntimeDir {
    pub fn new() -> Self {
        Self(private_dir())
    }
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    pub fn shutdown(&self, session: &str) {
        let path = self.path().join(session);
        if let Ok(welcome) = query(&path) {
            let Some(pid) = checked_server_pid(welcome.server_pid) else {
                eprintln!(
                    "test cleanup: refusing invalid server PID {}",
                    welcome.server_pid
                );
                return;
            };
            // Only signal a freshly queried server in this private directory.
            // TERM lets the server shut down/reap the managed child group.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            if !wait_for(Duration::from_secs(3), || {
                socket::lock_path_for_socket(&path)
                    .ok()
                    .and_then(|lock| socket::try_acquire_lock(&lock).ok())
                    .is_some_and(|held| held.is_some())
            }) {
                // Do not SIGKILL just the daemon: that orphans its managed child.
                eprintln!(
                    "test cleanup: server {} did not release {}",
                    welcome.server_pid,
                    path.display()
                );
            }
        }
    }

    /// Preserve abrupt hangup, but also clean its still-live managed group.
    pub fn crash(&self, session: &str) {
        let welcome = query(&self.path().join(session)).expect("query before crash");
        let child = checked_server_pid(welcome.child_pid).expect("valid managed child PID");
        let server = checked_server_pid(welcome.server_pid).expect("valid server PID");
        unsafe {
            libc::kill(server, libc::SIGKILL);
            if welcome.child_running && child > 0 && libc::getsid(child) == child {
                libc::kill(-child, libc::SIGKILL);
            }
        }
    }
}
impl Drop for RuntimeDir {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(self.path()) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    self.shutdown(name);
                }
            }
        }
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Shared CLI fixture. Environment variables are command-local, never global.
pub struct Env {
    dir: RuntimeDir,
}
impl Env {
    pub fn new() -> Self {
        Self {
            dir: RuntimeDir::new(),
        }
    }
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
    pub fn session(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }
    pub fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mn"));
        command.env("MNEME_SOCKET_DIR", self.dir());
        command
    }
    pub fn run(&self, args: &[&str], timeout: u64) -> (i32, String, String) {
        let mut child = ChildGuard(
            self.cmd()
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn mn"),
        );
        let out = collect_output(&mut child, Duration::from_secs(timeout));
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
    pub fn server_pid(&self, session: &str) -> Option<u32> {
        query(&self.dir().join(session)).ok().map(|w| w.server_pid)
    }
}

pub fn start_session(prefix: &str, shell_cmd: &str) -> (RuntimeDir, String, u32) {
    let dir = RuntimeDir::new();
    let session = format!("{prefix}-{}", std::process::id());
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mn"))
            .env("MNEME_SOCKET_DIR", dir.path())
            .args(["new", &session, "/bin/sh", "-c", shell_cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mn new"),
    );
    let output = collect_output(&mut child, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "mn new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = dir.path().join(&session);
    let mut pid = None;
    assert!(
        wait_for(Duration::from_secs(5), || {
            pid = query(&path).ok().map(|w| w.server_pid);
            pid.is_some()
        }),
        "server never became queryable"
    );
    (dir, session, pid.unwrap())
}

/// Reach live state under one absolute deadline, even with continuous replay.
pub fn attach_live(path: &Path, flags: protocol::ClientFlags) -> UnixStream {
    let stream = hello(
        path,
        &protocol::Hello {
            version: protocol::PROTOCOL_VERSION,
            intent: protocol::Intent::Attach,
            flags,
            rows: 24,
            cols: 80,
        },
    )
    .expect("attach Hello");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "ReplayEnd deadline exceeded");
        stream.set_read_timeout(Some(remaining)).unwrap();
        if recv_packet(&stream).unwrap().msg_type == protocol::MsgType::ReplayEnd {
            break;
        }
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}

/// Collect packets for a bounded interval, not an idle interval reset by each packet.
pub fn drain_packets(stream: &UnixStream, timeout: Duration) -> Vec<protocol::Packet> {
    let deadline = Instant::now() + timeout;
    let mut packets = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        stream.set_read_timeout(Some(remaining)).unwrap();
        match recv_packet(stream) {
            Ok(packet) => packets.push(packet),
            Err(_) => break,
        }
    }
    packets
}
