//! Control-plane progress while a registered client stops reading output.
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mneme::protocol::{
    self, ClientFlags, HEADER_SIZE, Hello, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(5);

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "control-plane deadline expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Unlike the fd protocol helper, read_exact respects a partial-read timeout.
fn recv(mut stream: &UnixStream) -> Packet {
    let mut header = [0; HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .expect("packet header before deadline");
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    assert!(len <= MAX_PAYLOAD);
    let mut payload = vec![0; len];
    stream
        .read_exact(&mut payload)
        .expect("packet payload before deadline");
    Packet::new(MsgType::from_u8(header[0]).unwrap(), payload)
}

fn through(stream: &UnixStream, expected: MsgType) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        stream
            .set_read_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("packet sequence deadline"),
            ))
            .unwrap();
        if recv(stream).msg_type == expected {
            break;
        }
    }
}

struct Session {
    child: Child,
    dir: TempDir,
}

impl Session {
    fn start() -> Self {
        let dir = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let (mut ready, notify) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(DEADLINE)).unwrap();
        let fd = notify.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mn"));
        command
            .args([
                "--server",
                "responsive",
                "--ready-fd",
                "3",
                "--ring-size",
                "65536",
                "--socket-path",
            ])
            .arg(dir.path().join("responsive"))
            .args([
                "--",
                "/bin/sh",
                "-c",
                "while [ ! -e \"$1\" ]; do sleep 0.01; done; exec cat /dev/zero",
                "fixture",
            ])
            .arg(dir.path().join("go"))
            .env("MNEME_PTY_LOG", dir.path().join("pty"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(notify);
        let session = Self { child, dir };
        let mut status = Vec::new();
        ready.read_to_end(&mut status).unwrap();
        assert!(!status.is_empty(), "server did not report readiness");
        session
    }

    fn hello(&self, intent: Intent) -> UnixStream {
        let stream = UnixStream::connect(self.dir.path().join("responsive")).unwrap();
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
        let welcome = recv(&stream);
        assert_eq!(welcome.msg_type, MsgType::Welcome);
        assert_eq!(welcome.parse_welcome().unwrap().server_pid, self.child.id());
        stream
    }

    fn attach(&self) -> UnixStream {
        let stream = self.hello(Intent::Attach);
        through(&stream, MsgType::ReplayEnd);
        through(&stream, MsgType::ResizeReq);
        stream
    }

    fn log_size(&self) -> u64 {
        std::fs::metadata(self.dir.path().join("pty"))
            .unwrap()
            .len()
    }

    fn flood_until_blocked(&self) {
        std::fs::write(self.dir.path().join("go"), "").unwrap();
        // Confirm ingestion has run and then actually stopped. This checks
        // producer throttling rather than relying on an arbitrary flood sleep.
        until(|| self.log_size() >= 32 * 1024);
        let mut last = self.log_size();
        let mut stable = Instant::now();
        until(|| {
            let size = self.log_size();
            if size != last {
                last = size;
                stable = Instant::now();
            }
            stable.elapsed() >= Duration::from_millis(200)
        });
    }

    fn kill_via_cli(&mut self) {
        let mut kill = Command::new(env!("CARGO_BIN_EXE_mn"))
            .env("MNEME_SOCKET_DIR", self.dir.path())
            .args(["kill", "responsive"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + DEADLINE;
        let result = loop {
            if let Some(status) = kill.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if result.is_none() {
            let _ = kill.kill();
            let _ = kill.wait();
        }
        assert!(result.expect("CLI kill timed out").success());
        until(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + DEADLINE;
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

#[test]
fn nonreading_client_does_not_block_query_attach_or_kill() {
    let mut session = Session::start();
    let _slow = session.attach();
    session.flood_until_blocked();
    let paused = session.log_size();
    for _ in 0..4 {
        drop(session.hello(Intent::Query));
    }
    let _second = session.attach();
    assert_eq!(
        session.log_size(),
        paused,
        "attach must not bypass backpressure"
    );
    session.kill_via_cli();
}

#[test]
fn detach_and_controller_transfer_work_with_full_output_queue() {
    let mut session = Session::start();
    let slow = session.attach();
    session.flood_until_blocked();
    let second = session.attach();
    // Removing the new controller elects slow again, even though its queue
    // cannot accept ResizeReq. A subsequent attach must still finish.
    protocol::send_packet(second.as_fd(), &Packet::empty(MsgType::Detach)).unwrap();
    // EOF confirms detach processing, not just receipt of a Welcome.
    let mut second_ref = &second;
    let mut byte = [0];
    assert_eq!(second_ref.read(&mut byte).unwrap(), 0);
    drop(session.hello(Intent::Query));
    let third = session.attach();
    let paused = session.log_size();
    protocol::send_packet(slow.as_fd(), &Packet::empty(MsgType::Detach)).unwrap();
    // Keep slow's read side open and non-reading: Detach alone must release
    // the full queue and cancel the stuck writer.
    through(&third, MsgType::Content);
    until(|| session.log_size() > paused);
    session.kill_via_cli();
}
