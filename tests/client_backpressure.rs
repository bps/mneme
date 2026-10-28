//! Exercise the actual CLI, not a raw protocol client. A private protocol-v1
//! peer lets us isolate CLI buffering from the server's own bounded queues.
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};

mod common;
use common::ChildGuard;
use std::time::{Duration, Instant};

use mneme::protocol::{HEADER_SIZE, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet, Welcome};
use mneme::socket::{SessionLock, lock_path_for_socket, try_acquire_lock};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(5);

fn until(ready: impl FnMut() -> bool) {
    assert!(common::wait_for(WAIT, ready), "deadline expired");
}

struct Client(ChildGuard);

impl Client {
    fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        until(|| {
            status = self.0.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }
}

struct Peer {
    dir: TempDir,
    listener: UnixListener,
    _lock: SessionLock,
}

impl Peer {
    fn new() -> Self {
        let dir = common::private_dir();
        let path = dir.path().join("client");
        let lock = try_acquire_lock(&lock_path_for_socket(&path).unwrap())
            .unwrap()
            .unwrap();
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            dir,
            listener,
            _lock: lock,
        }
    }

    fn spawn(&self, stdin: Stdio, stdout: Stdio) -> (Client, UnixStream) {
        let client = Client(ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_mn"))
                .env("MNEME_SOCKET_DIR", self.dir.path())
                .env_remove("MNEME_DEBUG")
                .env_remove("MNEME_STDOUT_LOG")
                .args(["attach", "client", "-q"])
                .stdin(stdin)
                .stdout(stdout)
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        ));
        let mut accepted = None;
        until(|| {
            match self.listener.accept() {
                Ok((s, _)) => accepted = Some(s),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("accept: {e}"),
            }
            accepted.is_some()
        });
        let mut stream = accepted.unwrap();
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Hello);
        send(
            &mut stream,
            Packet::welcome(&Welcome {
                version: PROTOCOL_VERSION,
                server_pid: std::process::id(),
                child_pid: std::process::id(),
                child_running: true,
                exit_status: 0,
                client_count: 1,
                ring_size: 0,
                ring_used: 0,
            }),
        );
        (client, stream)
    }
}

// Unlike the protocol's blocking fd helper, these operations have socket
// timeouts even after a partial header/payload.
fn recv(stream: &mut UnixStream) -> io::Result<Packet> {
    let mut header = [0; HEADER_SIZE];
    stream.read_exact(&mut header)?;
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    assert!(len <= MAX_PAYLOAD);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload)?;
    Ok(Packet::new(MsgType::from_u8(header[0]).unwrap(), payload))
}

fn send(stream: &mut UnixStream, packet: Packet) {
    stream.write_all(&packet.encode()).unwrap();
}

fn burst(kind: MsgType, count: usize) -> (Vec<u8>, Vec<u8>) {
    let mut wire = Vec::new();
    let mut bytes = Vec::new();
    for index in 0..count {
        // Nonuniform payload detects skipped, duplicated and reordered packets,
        // including partial writes at packet boundaries.
        let payload: Vec<u8> = (0..MAX_PAYLOAD)
            .map(|offset| ((index * 37 + offset) % 251) as u8)
            .collect();
        bytes.extend_from_slice(&payload);
        wire.extend_from_slice(&Packet::new(kind, payload).encode());
    }
    (wire, bytes)
}

// Keep writing until progress stops for 200 ms. An unbounded CLI reader would
// absorb this entire >8 MiB burst despite a blocked stdout; a bounded reader
// must instead leave bytes at the producer. No arbitrary single sleep decides
// whether backpressure has arrived.
fn stall(stream: &mut UnixStream, wire: &[u8]) -> usize {
    stream.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut last_progress = Instant::now();
    let mut sent = 0;
    while last_progress.elapsed() < Duration::from_millis(200) {
        assert!(Instant::now() < deadline, "producer did not settle");
        match stream.write(&wire[sent..]) {
            Ok(0) => panic!("peer closed while filling"),
            Ok(n) => {
                sent += n;
                assert!(
                    sent < wire.len(),
                    "CLI absorbed the entire burst with blocked stdout"
                );
                last_progress = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("fill: {e}"),
        }
    }
    stream.set_nonblocking(false).unwrap();
    sent
}

#[test]
fn blocked_stdout_bounds_reader_and_resumes_in_packet_order_through_exit() {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    let (mut wire, mut expected) = burst(MsgType::Replay, 2048);
    wire.extend_from_slice(&Packet::empty(MsgType::ReplayEnd).encode());
    let (live_wire, live_bytes) = burst(MsgType::Content, 128);
    wire.extend_from_slice(&live_wire);
    expected.extend_from_slice(&live_bytes);
    wire.extend_from_slice(&Packet::exit(23).encode());

    let sent = stall(&mut stream, &wire);
    assert!(
        client.0.try_wait().unwrap().is_none(),
        "Exit overtook blocked output"
    );

    let writer = std::thread::spawn(move || {
        stream.write_all(&wire[sent..]).unwrap();
        // Keep the peer alive until CLI exit; a second read sees EOF.
        while recv(&mut stream).is_ok() {}
    });
    let mut stdout = client.0.stdout.take().unwrap();
    let flags = rustix::fs::fcntl_getfl(&stdout).unwrap();
    rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut actual = Vec::new();
    let mut buf = [0; 8192];
    loop {
        assert!(Instant::now() < deadline, "stdout drain exceeded deadline");
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => actual.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("stdout: {e}"),
        }
    }
    assert_eq!(client.wait().code(), Some(23));
    writer.join().unwrap();
    assert_eq!(actual.len(), expected.len(), "missing or duplicated output");
    assert!(actual == expected, "output order/content changed");
}

fn detach_with_blocked_stdout(replay: bool) {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    if !replay {
        send(&mut stream, Packet::empty(MsgType::ReplayEnd));
        assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Resize);
    }
    let kind = if replay {
        MsgType::Replay
    } else {
        MsgType::Content
    };
    let (wire, _) = burst(kind, 2048);
    stall(&mut stream, &wire);
    let start = Instant::now();
    client.0.stdin.as_mut().unwrap().write_all(&[0x1c]).unwrap();
    assert!(client.wait().success());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "detach waited for stdout"
    );
    assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Detach);
}

#[test]
fn detach_responds_to_blocked_live_stdout() {
    detach_with_blocked_stdout(false);
}

#[test]
fn detach_responds_before_blocked_replay_end() {
    detach_with_blocked_stdout(true);
}

#[test]
fn signals_remain_responsive_with_blocked_stdout() {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Resize);
    let (wire, _) = burst(MsgType::Content, 2048);
    stall(&mut stream, &wire);

    assert_eq!(
        unsafe { libc::kill(client.0.id() as i32, libc::SIGWINCH) },
        0
    );
    assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Resize);
    assert_eq!(
        unsafe { libc::kill(client.0.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(client.wait().signal(), Some(libc::SIGTERM));
}

fn open_pty() -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    for fd in [&master, &slave] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).unwrap();
    }
    (master, slave)
}

#[test]
fn stopped_tty_detach_restores_termios_and_shared_descriptor_flags() {
    let (master, slave) = open_pty();
    let orig_flags = rustix::fs::fcntl_getfl(&slave).unwrap();
    let orig_termios = rustix::termios::tcgetattr(&slave).unwrap();
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(
        Stdio::from(slave.try_clone().unwrap()),
        Stdio::from(slave.try_clone().unwrap()),
    );
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Resize);
    assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOOFF) }, 0);
    let (wire, _) = burst(MsgType::Content, 2048);
    stall(&mut stream, &wire);
    assert_eq!(rustix::io::write(&master, &[0x1c]).unwrap(), 1);
    let status = client.wait();
    // Resume only AFTER exit: terminal restoration must not wait for drain.
    assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOON) }, 0);
    assert!(status.success());
    // Only NONBLOCK is ours to restore; macOS can add private descriptor
    // flags when kqueue registers a tty.
    assert_eq!(
        rustix::fs::fcntl_getfl(&slave).unwrap() & rustix::fs::OFlags::NONBLOCK,
        orig_flags & rustix::fs::OFlags::NONBLOCK,
    );
    let restored = rustix::termios::tcgetattr(&slave).unwrap();
    assert_eq!(restored.input_modes, orig_termios.input_modes);
    assert_eq!(restored.output_modes, orig_termios.output_modes);
    assert_eq!(restored.control_modes, orig_termios.control_modes);
    assert_eq!(restored.local_modes, orig_termios.local_modes);
    for index in [
        rustix::termios::SpecialCodeIndex::VMIN,
        rustix::termios::SpecialCodeIndex::VTIME,
    ] {
        assert_eq!(
            restored.special_codes[index],
            orig_termios.special_codes[index]
        );
    }
}

#[test]
fn replay_typeahead_is_preserved_but_not_forwarded_early() {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    client
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"typeahead")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let error = recv(&mut stream).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    stream.set_read_timeout(Some(WAIT)).unwrap();
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    let mut input = Vec::new();
    while input.len() < b"typeahead".len() {
        let packet = recv(&mut stream).unwrap();
        if packet.msg_type == MsgType::Content {
            input.extend_from_slice(&packet.payload);
        } else {
            assert_eq!(packet.msg_type, MsgType::Resize);
        }
    }
    assert_eq!(input, b"typeahead");
    send(&mut stream, Packet::exit(0));
    assert!(client.wait().success());
}

#[test]
fn pretyped_detach_does_not_skip_writable_replay() {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    client.0.stdin.as_mut().unwrap().write_all(&[0x1c]).unwrap();
    // Give the client time to observe stdin readiness before any replay packet.
    std::thread::sleep(Duration::from_millis(100));
    send(&mut stream, Packet::replay(b"snapshot before detach"));
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    assert!(client.wait().success());
    let mut bytes = Vec::new();
    client
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"snapshot before detach");
}

#[test]
fn writer_failure_does_not_override_ordered_output_and_exit() {
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(Stdio::piped(), Stdio::piped());
    // Simulate server Shutdown after it has decided the child exit status:
    // client resize writes fail, but the read half still has valid output/Exit.
    stream.shutdown(std::net::Shutdown::Read).unwrap();
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    send(&mut stream, Packet::empty(MsgType::ResizeReq));
    // Block stdout so writer failure is observed before the final Exit is read.
    let (mut wire, expected) = burst(MsgType::Content, 128);
    wire.extend_from_slice(&Packet::exit(19).encode());
    let writer = std::thread::spawn(move || {
        stream.write_all(&wire).unwrap();
    });
    std::thread::sleep(Duration::from_millis(100));
    // A closed outbound channel must not prematurely terminate the CLI.
    assert!(client.0.try_wait().unwrap().is_none());
    let mut actual = Vec::new();
    // The writer uses finite socket timeouts; the CLI must terminate after Exit.
    let stdout = client.0.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        stdout.read_to_end(&mut actual).unwrap();
        actual
    });
    assert_eq!(client.wait().code(), Some(19));
    writer.join().unwrap();
    assert!(reader.join().unwrap() == expected);
}

#[test]
fn busy_tty_receives_complete_reset_when_output_resumes_promptly() {
    let (master, slave) = open_pty();
    let flags = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let peer = Peer::new();
    let (mut client, mut stream) = peer.spawn(
        Stdio::from(slave.try_clone().unwrap()),
        Stdio::from(slave.try_clone().unwrap()),
    );
    send(&mut stream, Packet::empty(MsgType::ReplayEnd));
    assert_eq!(recv(&mut stream).unwrap().msg_type, MsgType::Resize);
    let (wire, _) = burst(MsgType::Content, 2048);
    stall(&mut stream, &wire);
    assert_eq!(rustix::io::write(&master, &[0x1c]).unwrap(), 1);
    // Keep the terminal busy briefly, then resume while cleanup's bounded
    // retry is still active. A single best-effort reset write loses these bytes.
    std::thread::sleep(Duration::from_millis(50));
    let reset = b"\x1b[?1049l\x1b[<u\x1b[0m\x1b[?25h\x1b[?7h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[r\r\n";
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    until(|| {
        match rustix::io::read(&master, &mut buf) {
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(rustix::io::Errno::AGAIN) => {}
            Err(e) => panic!("tty drain: {e}"),
        }
        bytes.windows(reset.len()).any(|part| part == reset)
    });
    assert!(client.wait().success());
}
