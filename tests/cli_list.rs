//! Tests for `mn list` (and bare `mn`) output formatting:
//! - empty list (no header printed)
//! - sort order
//! - status indicators: `*` (clients attached), `+` (child exited), ` ` (idle)

mod common;

use common::{Env, wait_for};
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

// ---------------------------------------------------------------------------

#[test]
fn list_empty_dir_prints_nothing() {
    let env = Env::new();
    let (code, out, err) = env.run(&["list"], 5);
    assert_eq!(code, 0, "list failed: {err}");
    assert!(out.is_empty(), "expected empty stdout, got: {out:?}");
}

#[test]
fn list_no_subcommand_equivalent_to_list() {
    let env = Env::new();
    let sess = env.session("ls-default");
    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0);
    assert!(wait_for(Duration::from_secs(2), || env
        .server_pid(&sess)
        .is_some()));

    let (code, out, err) = env.run(&[], 5);
    assert_eq!(code, 0, "bare mn failed: {err}");
    assert!(out.contains(&sess), "session missing from list: {out}");
    assert!(out.contains("Active sessions"), "missing header: {out}");
}

#[test]
fn list_sorts_sessions_alphabetically() {
    let env = Env::new();
    // Use an explicit prefix so order is predictable; the unique suffix
    // is the same shape (pid-counter), so sort order matches creation
    // order only because counters increment. To make the test robust
    // against sort-vs-creation differences, pick names whose last char
    // forces a specific order regardless of suffix.
    let s_a = format!("zzz-{}", env.session("a"));
    let s_b = format!("aaa-{}", env.session("a"));
    let s_c = format!("mmm-{}", env.session("a"));

    for s in [&s_a, &s_b, &s_c] {
        let (code, _, _) = env.run(&["new", s, "/bin/sh", "-c", "sleep 60"], 5);
        assert_eq!(code, 0);
    }
    for s in [&s_a, &s_b, &s_c] {
        assert!(wait_for(Duration::from_secs(2), || env
            .server_pid(s)
            .is_some()));
    }

    let (code, out, _err) = env.run(&["list"], 5);
    assert_eq!(code, 0);

    let pos_a = out.find(&s_a).expect("zzz line");
    let pos_b = out.find(&s_b).expect("aaa line");
    let pos_c = out.find(&s_c).expect("mmm line");
    // alphabetic: aaa < mmm < zzz
    assert!(pos_b < pos_c, "aaa should come before mmm: {out}");
    assert!(pos_c < pos_a, "mmm should come before zzz: {out}");
}

#[test]
fn list_marks_idle_session_with_blank() {
    let env = Env::new();
    let sess = env.session("ls-idle");
    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0);
    assert!(wait_for(Duration::from_secs(2), || env
        .server_pid(&sess)
        .is_some()));

    let (code, out, _) = env.run(&["list"], 5);
    assert_eq!(code, 0);
    // The idle marker is a literal space at start of the data line.
    // Lines look like: "  12345  session-name". Header "Active sessions"
    // does not contain the session name, so we can grep the session line.
    let line = out
        .lines()
        .find(|l| l.contains(&sess))
        .expect("session line");
    assert!(
        !line.starts_with('+') && !line.starts_with('*'),
        "idle session should not be marked + or *: {line:?}"
    );
}

#[test]
fn list_marks_attached_session_with_star() {
    let env = Env::new();
    let sess = env.session("ls-attached");

    // Start a session.
    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "sleep 60"], 5);
    assert_eq!(code, 0);
    assert!(wait_for(Duration::from_secs(2), || env
        .server_pid(&sess)
        .is_some()));

    // Manually attach a client (not via `mn attach`, since we don't have a TTY)
    // by speaking the protocol directly. The Welcome handshake bumps client_count.
    let path = env.dir().join(&sess);
    let stream = UnixStream::connect(&path).expect("connect");
    let hello = mneme::protocol::Hello {
        version: mneme::protocol::PROTOCOL_VERSION,
        intent: mneme::protocol::Intent::Attach,
        flags: mneme::protocol::ClientFlags::empty(),
        rows: 24,
        cols: 80,
    };
    mneme::protocol::send_packet(stream.as_fd(), &mneme::protocol::Packet::hello(&hello))
        .expect("send hello");
    // Read the Welcome to confirm we're counted as a client before listing.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set timeout");
    let _welcome = mneme::protocol::recv_packet(stream.as_fd()).expect("welcome");

    // Observe registration before asserting the CLI's attached marker.
    common::wait_for_client_count(&path, 1);

    let (code, out, _) = env.run(&["list"], 5);
    assert_eq!(code, 0);
    let line = out
        .lines()
        .find(|l| l.contains(&sess))
        .expect("session line");
    assert!(
        line.starts_with('*'),
        "attached session should start with '*': {line:?}"
    );

    // Detach cleanly so the Drop doesn't fight an active client.
    drop(stream);
}

#[test]
fn list_marks_exited_child_with_plus() {
    let env = Env::new();
    let sess = env.session("ls-exited");

    // Spawn a session whose child exits immediately. The server lingers
    // until a client picks up the exit status.
    let (code, _, _) = env.run(&["new", &sess, "/bin/sh", "-c", "true"], 5);
    assert_eq!(code, 0);

    // Wait for the child to actually exit and for the server's
    // child_running flag to flip. Query directly via the protocol.
    let path = env.dir().join(&sess);
    let exited = wait_for(Duration::from_secs(3), || {
        let Ok(stream) = UnixStream::connect(&path) else {
            return false;
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let hello = mneme::protocol::Hello {
            version: mneme::protocol::PROTOCOL_VERSION,
            intent: mneme::protocol::Intent::Query,
            flags: mneme::protocol::ClientFlags::empty(),
            rows: 0,
            cols: 0,
        };
        if mneme::protocol::send_packet(stream.as_fd(), &mneme::protocol::Packet::hello(&hello))
            .is_err()
        {
            return false;
        }
        let Ok(pkt) = mneme::protocol::recv_packet(stream.as_fd()) else {
            return false;
        };
        let Some(w) = pkt.parse_welcome() else {
            return false;
        };
        !w.child_running
    });
    assert!(exited, "child never reported as exited");

    let (code, out, _) = env.run(&["list"], 5);
    assert_eq!(code, 0);
    let line = out
        .lines()
        .find(|l| l.contains(&sess))
        .expect("session line");
    assert!(
        line.starts_with('+'),
        "exited-child session should start with '+': {line:?}"
    );
}

// Silence unused-import warnings on Linux-only code paths.
#[allow(dead_code)]
fn _force_use(_w: &mut std::io::Stdout) {
    let _: fn(&mut std::io::Stdout) -> std::io::Result<()> = |w| w.write_all(b"");
}
