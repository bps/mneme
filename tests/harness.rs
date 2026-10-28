//! Regressions for the shared harness itself.
mod common;

use common::{ChildGuard, RuntimeDir};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn server_signal_targets_exclude_special_pids() {
    for pid in [0, 1, std::process::id(), i32::MAX as u32 + 1, u32::MAX] {
        assert_eq!(common::checked_server_pid(pid), None);
    }
    assert_eq!(common::checked_server_pid(i32::MAX as u32), Some(i32::MAX));
}

#[test]
fn runtime_directory_is_short_and_private() {
    let dir = RuntimeDir::new();
    assert!(dir.path().as_os_str().len() < 40);
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn direct_child_is_reaped_on_panic() {
    let mut pid = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child = ChildGuard(Command::new("/bin/sleep").arg("60").spawn().unwrap());
        pid = child.id();
        panic!("intentional test panic");
    }));
    assert!(result.is_err());
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "direct child leaked"
    );
}

#[test]
fn reap_polling_has_a_deadline_for_a_live_child() {
    // A live child models the no-exit-status condition without requiring a
    // platform-specific kernel-exiting process. ChildGuard cleans it afterward.
    let mut child = ChildGuard(Command::new("/bin/sleep").arg("60").spawn().unwrap());
    let start = std::time::Instant::now();
    let error = common::reap_child(&mut child, Duration::from_millis(100)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "reap polling exceeded its bound"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "polling signalled the child"
    );
    let pid = child.id();
    drop(child);
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "normal cleanup failed to reap child"
    );
}

#[test]
fn collector_timeout_reaps_direct_child() {
    let mut child = Command::new("/bin/sleep")
        .arg("60")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::collect_output(&mut child, Duration::from_millis(100));
    }));
    assert!(result.is_err());
    assert!(
        child.try_wait().unwrap().is_some(),
        "collector failed to reap child"
    );
}

#[test]
fn raw_client_times_out_on_partial_packet() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let (reader, mut writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    writer
        .write_all(&[mneme::protocol::MsgType::Content as u8])
        .unwrap();
    // Keep the writer open: timeout, not EOF, must finish the partial header.
    let start = std::time::Instant::now();
    let error = match common::recv_packet(&reader) {
        Ok(_) => panic!("partial header accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "partial packet exceeded socket deadline"
    );
}

#[test]
fn probe_and_server_are_cleaned_on_panic() {
    let mut pids = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let probe =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/resize_probe.py");
        let (dir, session, _) =
            common::start_session("panic-probe", &format!("exec python3 {}", probe.display()));
        let path = dir.path().join(&session);
        let welcome = common::query(&path).unwrap();
        pids = Some((
            welcome.server_pid,
            common::checked_server_pid(welcome.child_pid).expect("managed child PID"),
        ));
        let stream = common::hello(
            &path,
            &mneme::protocol::Hello {
                version: mneme::protocol::PROTOCOL_VERSION,
                intent: mneme::protocol::Intent::Attach,
                flags: mneme::protocol::ClientFlags::READONLY,
                rows: 24,
                cols: 80,
            },
        )
        .unwrap();
        let mut text = Vec::new();
        assert!(
            common::wait_for(Duration::from_secs(5), || {
                if let Ok(packet) = common::recv_packet(&stream)
                    && matches!(
                        packet.msg_type,
                        mneme::protocol::MsgType::Replay | mneme::protocol::MsgType::Content
                    )
                {
                    text.extend_from_slice(&packet.payload);
                }
                text.windows(5).any(|window| window == b"READY")
            }),
            "probe was not initialized before panic"
        );
        drop(stream);
        panic!("intentional test panic");
    }));
    assert!(result.is_err());
    let (server, child) = pids.expect("session was not created");
    assert!(
        common::wait_for(Duration::from_secs(3), || unsafe {
            libc::kill(server as i32, 0) != 0 && libc::kill(child, 0) != 0
        }),
        "server {server} or managed probe {child} leaked after panic"
    );
}
