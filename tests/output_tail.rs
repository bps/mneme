//! Final PTY bytes must precede the terminal status, for live and replay clients.
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mneme::protocol::{self, ClientFlags, Hello, Intent, MsgType, PROTOCOL_VERSION, Packet};
use tempfile::TempDir;

const LIMIT: Duration = Duration::from_secs(12);
const BLOCKS: usize = 256;

mod common;
use common::recv_packet;

struct Session {
    child: Child,
    dir: TempDir,
}

impl Session {
    fn start(script: &str) -> Self {
        let dir = common::private_dir();
        let (mut ready, notify) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(LIMIT)).unwrap();
        let fd = notify.as_raw_fd();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mn"));
        cmd.args([
            "--server",
            "tail",
            "--ready-fd",
            "3",
            "--ring-size",
            "2097152",
            "--socket-path",
        ])
        .arg(dir.path().join("socket"))
        .args(["--", "/bin/sh", "-c", script, "fixture"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(dir.path().join("server.log")).unwrap(),
        ));
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(notify);
        let mut bytes = Vec::new();
        ready.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.first(), Some(&0), "startup failed: {bytes:?}");
        Self { child, dir }
    }

    fn connect(&self, intent: Intent) -> UnixStream {
        let stream = UnixStream::connect(self.dir.path().join("socket")).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent,
                flags: ClientFlags::READONLY,
                rows: 24,
                cols: 80,
            }),
        )
        .unwrap();
        assert_eq!(recv_packet(&stream).unwrap().msg_type, MsgType::Welcome);
        stream
    }

    fn query(&self) -> mneme::protocol::Welcome {
        let stream = UnixStream::connect(self.dir.path().join("socket")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent: Intent::Query,
                flags: ClientFlags::empty(),
                rows: 0,
                cols: 0,
            }),
        )
        .unwrap();
        recv_packet(&stream).unwrap().parse_welcome().unwrap()
    }

    fn release(&self) {
        std::fs::write(self.dir.path().join("go"), b"go").unwrap();
    }

    fn wait(&mut self, success: bool) {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success, "server status: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "server did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGCONT);
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn collect(stream: &UnixStream, mut bytes: Vec<u8>, mut replay_end: bool) -> Vec<u8> {
    let deadline = Instant::now() + LIMIT;
    loop {
        assert!(Instant::now() < deadline, "no terminal status");
        let packet = recv_packet(stream).unwrap();
        match packet.msg_type {
            MsgType::Replay => {
                assert!(!replay_end);
                bytes.extend(packet.payload);
            }
            MsgType::ReplayEnd => {
                assert!(!replay_end);
                replay_end = true;
            }
            MsgType::Content => {
                assert!(replay_end);
                bytes.extend(packet.payload);
            }
            MsgType::Exit => {
                assert!(replay_end);
                assert_eq!(packet.parse_exit_status(), Some(37));
                assert_eq!(
                    recv_packet(stream).unwrap_err().kind(),
                    std::io::ErrorKind::UnexpectedEof,
                    "socket must close immediately after Exit"
                );
                return bytes;
            }
            other => panic!("unexpected packet: {other:?}"),
        }
    }
}

#[test]
fn observed_exit_cannot_overtake_already_buffered_tail() {
    let mut session = Session::start(
        r#"
echo $$ > "$1/pid"
while [ ! -e "$1/go" ]; do sleep 0.01; done
printf FINAL_TAIL
exit 37
"#,
    );
    let stream = session.connect(Intent::Attach);
    assert_eq!(recv_packet(&stream).unwrap().msg_type, MsgType::ReplayEnd);
    // Stop only this fixture server. The child can finish while the mainloop
    // cannot read: both SIGCHLD and the final PTY bytes are ready on resume.
    assert_eq!(
        unsafe { libc::kill(session.child.id() as i32, libc::SIGSTOP) },
        0
    );
    session.release();
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Ok(pid) = std::fs::read_to_string(session.dir.path().join("pid")) {
            let status = Command::new("ps")
                .args(["-o", "stat=", "-p", pid.trim()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&status.stdout);
            // Darwin can remain in exit/terminal-flush state (E) until
            // the stopped server consumes the tail. Linux reports Z.
            if state.contains('Z') || state.contains('E') {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fixture child did not reach exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        unsafe { libc::kill(session.child.id() as i32, libc::SIGCONT) },
        0
    );
    assert_eq!(collect(&stream, Vec::new(), true), b"FINAL_TAIL");
    session.wait(true);
}

const BURST: &str = r#"
while [ ! -e "$1/go" ]; do sleep 0.01; done
i=0
while [ "$i" -lt 256 ]; do printf '%04096d' 7; i=$((i + 1)); done
printf FINAL_TAIL
exit 37
"#;

fn expected() -> Vec<u8> {
    let block = format!("{:04096}", 7);
    let mut bytes = block.repeat(BLOCKS).into_bytes();
    bytes.extend_from_slice(b"FINAL_TAIL");
    bytes
}

#[test]
fn live_burst_tail_precedes_exit_and_socket_close() {
    let mut session = Session::start(BURST);
    let stream = session.connect(Intent::Attach);
    assert_eq!(recv_packet(&stream).unwrap().msg_type, MsgType::ReplayEnd);
    session.release();
    assert_eq!(collect(&stream, Vec::new(), true), expected());
    session.wait(true);
}

#[test]
fn stalled_replay_at_exit_is_bounded_and_remains_queryable() {
    let mut session = Session::start(
        r#"
i=0
while [ "$i" -lt 256 ]; do printf '%04096d' 7; i=$((i + 1)); done
while [ ! -e "$1/go" ]; do sleep 0.01; done
printf FINAL_TAIL
exit 37
"#,
    );
    let deadline = Instant::now() + LIMIT;
    while session.query().ring_used < (BLOCKS * 4096) as u32 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stream = session.connect(Intent::Attach);
    // Do not drain replay. The child can still exit because the new tail fits
    // in the outbound queue, independently of the blocked snapshot writer.
    session.release();
    loop {
        session.query(); // Each query has its own one-second response deadline.
        let log = std::fs::read_to_string(session.dir.path().join("server.log")).unwrap();
        if log.contains("undelivered output") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stalled writer not bounded: {log}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    while let Ok(packet) = recv_packet(&stream) {
        assert_ne!(
            packet.msg_type,
            MsgType::Exit,
            "stalled replay reported success"
        );
    }
    drop(stream);
    // An unconfirmed notification must not discard the retained final output.
    let retry = session.connect(Intent::Attach);
    assert_eq!(collect(&retry, Vec::new(), false), expected());
    session.wait(true);
}

#[test]
fn explicit_shutdown_bounds_an_exited_sessions_blocked_replay() {
    let mut session = Session::start(BURST);
    session.release();
    let deadline = Instant::now() + LIMIT;
    while session.query().child_running {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stream = session.connect(Intent::Attach);
    let start = Instant::now();
    assert_eq!(
        unsafe { libc::kill(session.child.id() as i32, libc::SIGTERM) },
        0
    );
    session.wait(true);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "shutdown exceeded CLI kill deadline"
    );
    drop(stream);
}

#[test]
fn late_attach_receives_entire_burst_before_status() {
    let mut session = Session::start(BURST);
    session.release();
    // Query until exit observation, without consuming retained output.
    let deadline = Instant::now() + LIMIT;
    loop {
        let stream = UnixStream::connect(session.dir.path().join("socket")).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent: Intent::Query,
                flags: ClientFlags::empty(),
                rows: 0,
                cols: 0,
            }),
        )
        .unwrap();
        let welcome = recv_packet(&stream).unwrap().parse_welcome().unwrap();
        if !welcome.child_running {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stream = session.connect(Intent::Attach);
    assert_eq!(collect(&stream, Vec::new(), false), expected());
    session.wait(true);
}
