//! Managed process lifetime checks. Every fixture is synchronized by a PID
//! file, and every wait has a deadline. The server is our direct child so its
//! disappearance does not depend on the host's orphan reaper.
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mneme::protocol::{self, ClientFlags, Hello, Intent, MsgType, PROTOCOL_VERSION, Packet};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(5);

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "deadline expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn gone(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

struct Session {
    server: Child,
    dir: TempDir,
    pids: Vec<i32>,
    managed_group: i32,
}

impl Session {
    fn start(shell: &str, script: &str) -> Self {
        let dir = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let (mut ready, notify) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(DEADLINE)).unwrap();
        let notify_fd = notify.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mn"));
        command
            .args(["--server", "lifecycle", "--ready-fd", "3", "--socket-path"])
            .arg(dir.path().join("session"))
            .args(["--", shell, "-c", script, "fixture"])
            .arg(dir.path().join("pids"))
            .arg(dir.path().join("handled"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(notify_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let server = command.spawn().unwrap();
        drop(notify);
        let mut session = Self {
            server,
            dir,
            pids: Vec::new(),
            managed_group: 0,
        };
        let mut bytes = Vec::new();
        ready.read_to_end(&mut bytes).unwrap();
        // PID file is published only after the fixture's signal handlers and
        // descendants are ready. Server readiness alone does not establish that.
        until(|| {
            let Ok(text) = std::fs::read_to_string(session.dir.path().join("pids")) else {
                return false;
            };
            session.pids = text
                .split_whitespace()
                .map(|s| s.parse().unwrap())
                .collect();
            !session.pids.is_empty()
        });
        session.managed_group = unsafe { libc::getsid(session.pids[0]) };
        assert!(session.managed_group > 0);
        session
    }

    fn hello(&self, intent: Intent) -> UnixStream {
        let stream = UnixStream::connect(self.dir.path().join("session")).unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        stream.set_write_timeout(Some(DEADLINE)).unwrap();
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent,
                flags: ClientFlags::empty(),
                rows: 24,
                cols: 80,
            }),
        )
        .unwrap();
        assert_eq!(
            protocol::recv_packet(stream.as_fd()).unwrap().msg_type,
            MsgType::Welcome
        );
        stream
    }

    fn terminate(&mut self) {
        let start = Instant::now();
        assert_eq!(
            unsafe { libc::kill(self.server.id() as i32, libc::SIGTERM) },
            0
        );
        until(|| self.server.try_wait().unwrap().is_some());
        assert!(
            self.server.wait().unwrap().success(),
            "server cleanup failed"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "exceeded CLI kill deadline"
        );
        for &pid in &self.pids {
            until(|| gone(pid));
        }
        until(|| gone(self.managed_group));
        until(|| gone(-self.managed_group));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.server.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.server.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.server.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.server.kill();
            let _ = self.server.wait();
        }
    }
}

#[test]
fn term_reaps_child_and_terminates_descendant_group() {
    let mut session = Session::start(
        "/bin/sh",
        r#"
        sleep 60 &
        job=$!
        trap 'kill "$job" 2>/dev/null; wait "$job" 2>/dev/null; echo handled > "$2"; exit 0' HUP TERM
        echo $$ $job > "$1"
        wait "$job"
    "#,
    );
    session.terminate();
    assert!(
        session.dir.path().join("handled").exists(),
        "no orderly shutdown"
    );
}

#[test]
fn term_escalates_for_signal_resistant_child() {
    let mut session = Session::start(
        "/bin/sh",
        r#"
        trap '' HUP TERM
        echo $$ > "$1"
        while :; do :; done
    "#,
    );
    let start = Instant::now();
    session.terminate();
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "did not exercise escalation"
    );
}

#[test]
fn child_does_not_inherit_ignored_hup() {
    let mut session = Session::start(
        "/bin/sh",
        r#"
        echo $$ > "$1"
        while :; do :; done
    "#,
    );
    assert_eq!(unsafe { libc::kill(session.pids[0], libc::SIGHUP) }, 0);
    until(|| gone(session.pids[0]));
    // No attach occurred: exited-session output must remain queryable.
    let stream = session.hello(Intent::Query);
    drop(stream);
    assert!(session.server.try_wait().unwrap().is_none());
    session.terminate();
}

#[test]
fn detach_and_server_hup_preserve_running_session() {
    let mut session = Session::start(
        "/bin/sh",
        r#"
        echo $$ > "$1"
        while :; do :; done
    "#,
    );
    let stream = session.hello(Intent::Attach);
    loop {
        if protocol::recv_packet(stream.as_fd()).unwrap().msg_type == MsgType::ReplayEnd {
            break;
        }
    }
    protocol::send_packet(stream.as_fd(), &Packet::empty(MsgType::Detach)).unwrap();
    drop(stream);
    assert_eq!(
        unsafe { libc::kill(session.server.id() as i32, libc::SIGHUP) },
        0
    );
    drop(session.hello(Intent::Query));
    assert!(!gone(session.pids[0]));
    session.terminate();
}

#[test]
fn term_reaches_signal_resistant_active_foreground_group() {
    let dir = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let script_path = dir.path().join("foreground.sh");
    std::fs::write(
        &script_path,
        r#"
        trap '' HUP TERM
        echo $$ > "$1"
        while :; do :; done
    "#,
    )
    .unwrap();
    let script = format!("set -m; /bin/sh '{}' \"$1\"", script_path.display());
    let mut session = Session::start("/bin/bash", &script);
    let foreground = session.pids[0];
    assert_ne!(
        unsafe { libc::getpgid(foreground) },
        unsafe { libc::getsid(foreground) },
        "fixture must run outside the shell's process group"
    );
    session.terminate();
    until(|| gone(-foreground));
}

#[test]
fn term_remains_responsive_with_nonreading_client() {
    let mut session = Session::start(
        "/bin/sh",
        r#"
        trap '' HUP TERM
        echo $$ > "$1"
        while :; do printf '%4096s' x; done
    "#,
    );
    let stream = session.hello(Intent::Attach);
    // Wait until socket data is queued without draining it. Output continues
    // until the socket and bounded outbound channel apply backpressure.
    until(|| {
        let mut pending = 0;
        unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut pending) == 0 && pending > 0 }
    });
    std::thread::sleep(Duration::from_millis(200));
    session.terminate();
    drop(stream);
}
