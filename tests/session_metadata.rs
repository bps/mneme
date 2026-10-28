//! Welcome metadata comes from mainloop state when Hello is handled, not accept.
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mneme::protocol::{
    self, ClientFlags, HEADER_SIZE, Hello, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
    Welcome,
};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(5);

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "metadata deadline expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// read_exact respects timeouts even for partial packets.
fn recv(mut stream: &UnixStream) -> Packet {
    let mut header = [0; HEADER_SIZE];
    stream.read_exact(&mut header).unwrap();
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    assert!(len <= MAX_PAYLOAD);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).unwrap();
    Packet::new(MsgType::from_u8(header[0]).unwrap(), payload)
}

fn through(stream: &UnixStream, expected: MsgType) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        // The socket already has a bounded read timeout. Do not reset socket
        // options here: macOS can reject them after the server has closed,
        // even while its final ReplayEnd/Exit packets remain readable.
        assert!(Instant::now() < deadline, "packet sequence deadline");
        if recv(stream).msg_type == expected {
            return;
        }
    }
}

struct Session {
    server: Child,
    dir: TempDir,
}

impl Session {
    fn start(script: &str) -> Self {
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
                "metadata",
                "--ready-fd",
                "3",
                "--ring-size",
                "8388608",
                "--socket-path",
            ])
            .arg(dir.path().join("metadata"))
            .args(["--", "/bin/sh", "-c", script, "fixture"])
            .arg(dir.path())
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
        let server = command.spawn().unwrap();
        drop(notify);
        let session = Self { server, dir };
        let mut status = Vec::new();
        ready.read_to_end(&mut status).unwrap();
        assert_eq!(status, [0], "server readiness failed");
        session
    }

    fn idle() -> Self {
        Self::start(
            "echo $$ > \"$1/pid\"; while [ ! -e \"$1/exit\" ]; do sleep 0.01; done; exit 23",
        )
    }

    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(self.dir.path().join("metadata")).unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        stream.set_write_timeout(Some(DEADLINE)).unwrap();
        stream
    }

    fn hello(stream: &UnixStream, intent: Intent, flags: ClientFlags) -> Welcome {
        protocol::send_packet(
            stream.as_fd(),
            &Packet::hello(&Hello {
                version: PROTOCOL_VERSION,
                intent,
                flags,
                rows: 24,
                cols: 80,
            }),
        )
        .unwrap();
        recv(stream).parse_welcome().expect("Welcome")
    }

    fn query(&self) -> Welcome {
        let mut stream = self.connect();
        let welcome = Self::hello(&stream, Intent::Query, ClientFlags::empty());
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "query sent replay or live data"
        );
        welcome
    }

    fn attach(&self, flags: ClientFlags) -> (UnixStream, Welcome) {
        let stream = self.connect();
        let welcome = Self::hello(&stream, Intent::Attach, flags);
        through(&stream, MsgType::ReplayEnd);
        if !flags.intersects(ClientFlags::READONLY | ClientFlags::LOW_PRIORITY) {
            through(&stream, MsgType::ResizeReq);
        }
        (stream, welcome)
    }

    fn count(&self, expected: u16) {
        until(|| self.query().client_count == expected);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.server.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.server.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + DEADLINE;
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

#[test]
fn welcome_reports_child_pid_and_retains_it_after_exit() {
    let session = Session::idle();
    let mut pid = None;
    until(|| {
        pid = std::fs::read_to_string(session.dir.path().join("pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        pid.is_some()
    });
    let welcome = session.query();
    assert_eq!(welcome.server_pid, session.server.id());
    assert_eq!(welcome.child_pid, pid.unwrap());
    assert_ne!(welcome.child_pid, welcome.server_pid);
    assert!(welcome.child_running);
    assert_eq!(welcome.client_count, 0);
    // Keep an accepted socket idle while authoritative child state changes.
    let delayed = session.connect();
    assert!(session.query().child_running); // also proves idle intake cannot block queries
    std::fs::write(session.dir.path().join("exit"), "").unwrap();
    until(|| !session.query().child_running);
    let exited = Session::hello(&delayed, Intent::Query, ClientFlags::empty());
    assert!(!exited.child_running);
    assert_eq!(exited.exit_status, 23);
    assert_eq!(exited.child_pid, welcome.child_pid);
    assert_eq!(exited.client_count, 0);
    // Queries must not consume the retained exit result.
    let attach = session.connect();
    let retained = Session::hello(&attach, Intent::Attach, ClientFlags::READONLY);
    assert!(!retained.child_running);
    assert_eq!(retained.exit_status, 23);
    assert_eq!(retained.child_pid, welcome.child_pid);
    assert_eq!(retained.client_count, 1);
    through(&attach, MsgType::ReplayEnd);
    let exit = recv(&attach);
    assert_eq!(exit.msg_type, MsgType::Exit);
    assert_eq!(exit.parse_exit_status(), Some(23));
}

#[test]
fn counts_include_readonly_and_exclude_queries_and_disconnected_clients() {
    let session = Session::idle();
    let delayed = session.connect();
    assert_eq!(session.query().client_count, 0);
    let (first, welcome) = session.attach(ClientFlags::empty());
    assert_eq!(welcome.client_count, 1);
    let (second, welcome) = session.attach(ClientFlags::READONLY);
    assert_eq!(welcome.client_count, 2);
    let current = Session::hello(&delayed, Intent::Query, ClientFlags::empty());
    assert_eq!(
        current.client_count, 2,
        "accepted sockets must not retain old counts"
    );
    for _ in 0..8 {
        assert_eq!(session.query().client_count, 2);
    }
    protocol::send_packet(second.as_fd(), &Packet::empty(MsgType::Detach)).unwrap();
    session.count(1);
    drop(first);
    session.count(0);
    // A peer disappearing before Welcome cannot leave a permanent registration.
    let failed = session.connect();
    protocol::send_packet(
        failed.as_fd(),
        &Packet::hello(&Hello {
            version: PROTOCOL_VERSION,
            intent: Intent::Attach,
            flags: ClientFlags::empty(),
            rows: 24,
            cols: 80,
        }),
    )
    .unwrap();
    failed.shutdown(std::net::Shutdown::Both).unwrap();
    drop(failed);
    session.count(0);
    let (_client, welcome) = session.attach(ClientFlags::empty());
    assert_eq!(welcome.client_count, 1);
}

#[test]
fn queries_do_not_change_controller_or_resize_state() {
    let session = Session::idle();
    let (first, _) = session.attach(ClientFlags::empty());
    let (second, _) = session.attach(ClientFlags::empty());
    for _ in 0..8 {
        assert_eq!(session.query().client_count, 2);
    }
    for mut stream in [&first, &second] {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0];
        let error = stream
            .read(&mut byte)
            .expect_err("query triggered a controller packet");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
    }
    // Removing the actual controller must still elect the first client.
    protocol::send_packet(second.as_fd(), &Packet::empty(MsgType::Detach)).unwrap();
    first.set_read_timeout(Some(DEADLINE)).unwrap();
    assert_eq!(recv(&first).msg_type, MsgType::ResizeReq);
    session.count(1);
}

#[test]
fn replaying_registration_counts_and_delayed_hello_gets_current_ring_usage() {
    let session = Session::start(
        "while [ ! -e \"$1/go\" ]; do sleep 0.01; done; head -c 4194304 /dev/zero; while :; do sleep 1; done",
    );
    let delayed = session.connect();
    assert_eq!(session.query().ring_used, 0);
    std::fs::write(session.dir.path().join("go"), "").unwrap();
    until(|| session.query().ring_used == 4 * 1024 * 1024);
    let current = Session::hello(&delayed, Intent::Query, ClientFlags::empty());
    assert_eq!(current.ring_size, 8 * 1024 * 1024);
    assert_eq!(current.ring_used, 4 * 1024 * 1024);
    let slow = session.connect();
    rustix::net::sockopt::set_socket_recv_buffer_size(&slow, 4096).unwrap();
    let welcome = Session::hello(&slow, Intent::Attach, ClientFlags::READONLY);
    assert_eq!(welcome.client_count, 1);
    assert_eq!(welcome.ring_used, current.ring_used);
    // A multi-megabyte snapshot cannot fit the Unix socket buffer; the writer
    // remains in replay while queries and another registration are processed.
    assert_eq!(session.query().client_count, 1);
    let second = session.connect();
    assert_eq!(
        Session::hello(&second, Intent::Attach, ClientFlags::READONLY).client_count,
        2
    );
    assert_eq!(session.query().client_count, 2);
    drop(slow);
    drop(second);
    session.count(0);
}
