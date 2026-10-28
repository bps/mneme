//! Internal startup status pipe. This is separate from the session protocol.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;
const MAX_ERROR: usize = 4095;
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);

// Protocol v1 advertises ring capacity and length as u32. Vec allocations
// must also fit in isize on platforms with a smaller address space.
pub const MAX_RING_SIZE: usize = if (u32::MAX as usize) < (isize::MAX as usize) {
    u32::MAX as usize
} else {
    isize::MAX as usize
};

pub fn validate_ring_size(size: usize) -> Result<(), String> {
    if size == 0 || size > MAX_RING_SIZE {
        return Err(format!(
            "ring size must be between 1 and {MAX_RING_SIZE} bytes"
        ));
    }
    Ok(())
}

pub struct Reporter(Option<File>);

impl Reporter {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        // The fd must survive the server exec, but not the PTY child exec.
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        Ok(Self(Some(fd.into())))
    }

    pub fn success(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.0.take() {
            file.write_all(&[SUCCESS])?;
        }
        Ok(())
    }

    pub fn failure(&mut self, message: &str) {
        if let Some(mut file) = self.0.take() {
            let bytes = message.as_bytes();
            let mut frame = Vec::with_capacity(1 + bytes.len().min(MAX_ERROR));
            frame.push(FAILURE);
            frame.extend_from_slice(&bytes[..bytes.len().min(MAX_ERROR)]);
            // A disconnected CLI cannot receive an error; retain the original
            // setup error for the server log rather than replacing it.
            let _ = file.write_all(&frame);
        }
    }
}

pub fn wait(fd: &OwnedFd, timeout: Duration) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    let deadline = Instant::now() + timeout;
    let mut frame = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "server readiness deadline exceeded",
            ));
        }
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as _,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let mut buf = [0; 4096];
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => {
                let message = if frame.first() == Some(&FAILURE) {
                    String::from_utf8_lossy(&frame[1..]).into_owned()
                } else {
                    "server exited before reporting readiness".into()
                };
                return Err(io::Error::other(message));
            }
            Ok(n) => {
                frame.extend_from_slice(&buf[..n]);
                match frame[0] {
                    SUCCESS if frame.len() == 1 => return Ok(()),
                    FAILURE if frame.len() <= MAX_ERROR => {}
                    FAILURE => {
                        return Err(io::Error::other(
                            String::from_utf8_lossy(&frame[1..=MAX_ERROR]).into_owned(),
                        ));
                    }
                    _ => return Err(io::Error::other("invalid server readiness status")),
                }
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn explicit_success_is_required() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        drop(writer);
        let err = wait(&reader, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("before reporting readiness"));
    }

    #[test]
    fn success_does_not_require_eof() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&writer, &[SUCCESS]).unwrap();
        wait(&reader, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn fragmented_failure_is_collected() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            rustix::io::write(&writer, &[FAILURE]).unwrap();
            tx.send(()).unwrap();
            rustix::io::write(&writer, b"setup failed").unwrap();
        });
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let err = wait(&reader, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.to_string(), "setup failed");
        thread.join().unwrap();
    }

    #[test]
    fn pty_child_cannot_keep_status_pipe_open() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        let mut reporter = Reporter::new(writer).unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        // This must produce EOF even while the spawned process is alive.
        reporter.failure("setup failed after spawn");
        let result = wait(&reader, Duration::from_secs(1));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result.unwrap_err().to_string(), "setup failed after spawn");
    }

    #[test]
    fn readiness_has_a_deadline_with_no_status() {
        let (reader, _writer) = rustix::pipe::pipe().unwrap();
        let err = wait(&reader, Duration::from_millis(30)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn readiness_has_a_deadline_even_with_partial_status() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&writer, &[FAILURE]).unwrap();
        let start = Instant::now();
        let err = wait(&reader, Duration::from_millis(30)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn errors_are_bounded() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        Reporter::new(writer).unwrap().failure(&"x".repeat(8192));
        let err = wait(&reader, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.to_string().len(), MAX_ERROR);
    }

    #[test]
    fn invalid_status_is_rejected() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&writer, b"?").unwrap();
        assert!(wait(&reader, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn capacity_matches_protocol_and_platform_limits() {
        assert!(validate_ring_size(0).is_err());
        assert!(validate_ring_size(1).is_ok());
        assert!(validate_ring_size(MAX_RING_SIZE).is_ok());
        assert!(validate_ring_size(MAX_RING_SIZE + 1).is_err());
    }
}
