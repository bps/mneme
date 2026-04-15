mod client;
mod protocol;
mod ring;
mod server;
mod socket;

use std::env;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Internal server mode — not user-facing
    if args.get(1).map(|s| s.as_str()) == Some("--server") {
        match server::run_server(&args[2..]) {
            Ok(()) => process::exit(0),
            Err(e) => {
                eprintln!("mneme: server error: {e}");
                process::exit(1);
            }
        }
    }

    match run_cli(&args[1..]) {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("mneme: {e}");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CliOpts {
    action: Action,
    session_name: String,
    command: Vec<String>,
    detach_key: u8,
    readonly: bool,
    low_priority: bool,
    quiet: bool,
    force: bool,
    ring_size: usize,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    List,
    Create,       // -c: create + attach
    CreateOnly,   // -n: create, don't attach
    Attach,       // -a: attach only
    AttachOrCreate, // -A: attach if exists, else create
}

const DEFAULT_RING_SIZE: usize = 1024 * 1024; // 1 MiB
const DEFAULT_DETACH_KEY: u8 = 0x1C; // Ctrl-backslash

fn parse_args(args: &[String]) -> Result<CliOpts, String> {
    let mut action = None;
    let mut session_name = None;
    let mut detach_key = DEFAULT_DETACH_KEY;
    let mut readonly = false;
    let mut low_priority = false;
    let mut quiet = false;
    let mut force = false;
    let mut ring_size = DEFAULT_RING_SIZE;
    let mut command = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => action = Some(Action::Create),
            "-n" => action = Some(Action::CreateOnly),
            "-a" => action = Some(Action::Attach),
            "-A" => action = Some(Action::AttachOrCreate),
            "-r" => readonly = true,
            "-l" => low_priority = true,
            "-q" => quiet = true,
            "-f" => force = true,
            "-e" => {
                i += 1;
                let key_str = args.get(i).ok_or("-e requires an argument")?;
                detach_key = parse_detach_key(key_str)?;
            }
            "-s" => {
                i += 1;
                let size_str = args.get(i).ok_or("-s requires an argument")?;
                ring_size = parse_size(size_str)?;
            }
            other => {
                if session_name.is_none() {
                    session_name = Some(other.to_string());
                } else {
                    // Everything after session name is the command
                    command = args[i..].to_vec();
                    break;
                }
            }
        }
        i += 1;
    }

    // No arguments at all → list
    if action.is_none() && session_name.is_none() {
        return Ok(CliOpts {
            action: Action::List,
            session_name: String::new(),
            command: Vec::new(),
            detach_key,
            readonly,
            low_priority,
            quiet,
            force,
            ring_size,
        });
    }

    let action = action.ok_or("usage: mneme [-a|-A|-c|-n] [-r] [-q] [-f] [-e key] [-s size] name [command...]")?;
    let session_name = session_name.ok_or("session name required")?;
    socket::validate_session_name(&session_name)?;

    Ok(CliOpts {
        action,
        session_name,
        command,
        detach_key,
        readonly,
        low_priority,
        quiet,
        force,
        ring_size,
    })
}

fn parse_detach_key(s: &str) -> Result<u8, String> {
    if s.len() == 2 && s.starts_with('^') {
        let ch = s.as_bytes()[1];
        Ok(ch & 0x1F)
    } else if s.len() == 1 {
        Ok(s.as_bytes()[0])
    } else {
        Err(format!("invalid detach key: {s}"))
    }
}

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num_str, multiplier) = if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1024 * 1024)
    } else if s.ends_with('K') || s.ends_with('k') {
        (&s[..s.len() - 1], 1024)
    } else {
        (s, 1)
    };
    let n: usize = num_str
        .parse()
        .map_err(|_| format!("invalid size: {s}"))?;
    Ok(n * multiplier)
}

// ---------------------------------------------------------------------------
// CLI dispatch
// ---------------------------------------------------------------------------

fn run_cli(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let opts = parse_args(args).map_err(|e| e.to_string())?;

    match opts.action {
        Action::List => {
            list_sessions()?;
            Ok(0)
        }
        Action::Create => {
            create_session(&opts)?;
            attach_session(&opts)
        }
        Action::CreateOnly => {
            create_session(&opts)?;
            Ok(0)
        }
        Action::Attach => attach_session(&opts),
        Action::AttachOrCreate => {
            if session_exists(&opts.session_name)? {
                attach_session(&opts)
            } else {
                create_session(&opts)?;
                attach_session(&opts)
            }
        }
    }
}

fn default_command() -> Vec<String> {
    if let Ok(cmd) = env::var("MNEME_CMD") {
        return vec!["/bin/sh".into(), "-c".into(), cmd];
    }
    if let Ok(shell) = env::var("SHELL") {
        return vec![shell];
    }
    vec!["/bin/sh".into()]
}

fn get_terminal_size() -> (u16, u16) {
    use rustix::termios::tcgetwinsize;
    match tcgetwinsize(std::io::stdin()) {
        Ok(ws) => (ws.ws_row, ws.ws_col),
        Err(_) => (24, 80),
    }
}

fn create_session(opts: &CliOpts) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = socket::socket_path(&opts.session_name)?;
    let (rows, cols) = get_terminal_size();
    let cmd = if opts.command.is_empty() {
        default_command()
    } else {
        opts.command.clone()
    };

    // Create a pipe for readiness signaling.
    // Use libc directly to avoid CLOEXEC on the write end —
    // the server (child) needs the write end to survive exec.
    let (cli_fd, server_raw_fd) = {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Set CLOEXEC on the read end (CLI keeps it, doesn't need to pass it)
        unsafe {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // Do NOT set CLOEXEC on fds[1] — the server needs it
        (unsafe { OwnedFd::from_raw_fd(fds[0]) }, fds[1])
    };

    // Build server args
    let exe = env::current_exe()?;
    let mut server_args = vec![
        "--server".into(),
        opts.session_name.clone(),
        "--ready-fd".into(),
        format!("{server_raw_fd}"),
        "--rows".into(),
        format!("{rows}"),
        "--cols".into(),
        format!("{cols}"),
        "--ring-size".into(),
        format!("{}", opts.ring_size),
        "--socket-path".into(),
        socket_path.to_string_lossy().to_string(),
        "--".into(),
    ];
    server_args.extend(cmd);

    // Spawn the server process
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(&exe);
    command
        .args(&server_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);

    let _child = command.spawn()?;

    // Close the write end in the parent so only the server has it
    unsafe { libc::close(server_raw_fd); }

    // Wait for readiness: EOF = ready, data = error
    let mut error_buf = vec![0u8; 4096];
    let n = rustix::io::read(&cli_fd, &mut error_buf)?;
    if n > 0 {
        let msg = String::from_utf8_lossy(&error_buf[..n]);
        return Err(format!("server startup failed: {msg}").into());
    }

    if !opts.quiet {
        eprintln!("mneme: {}: session created", opts.session_name);
    }
    Ok(())
}

fn attach_session(opts: &CliOpts) -> Result<i32, Box<dyn std::error::Error>> {
    let socket_path = socket::socket_path(&opts.session_name)?;
    let flags = {
        let mut f = protocol::ClientFlags::empty();
        if opts.readonly {
            f |= protocol::ClientFlags::READONLY;
        }
        if opts.low_priority {
            f |= protocol::ClientFlags::LOW_PRIORITY;
        }
        f
    };
    let (rows, cols) = get_terminal_size();

    let status = client::attach(
        &socket_path,
        flags,
        rows,
        cols,
        opts.detach_key,
        opts.quiet,
    )?;

    match status {
        client::AttachResult::Detached => {
            if !opts.quiet {
                eprintln!("mneme: {}: detached", opts.session_name);
            }
            Ok(0)
        }
        client::AttachResult::Exited(code) => {
            if !opts.quiet {
                eprintln!(
                    "mneme: {}: session terminated with exit status {code}",
                    opts.session_name
                );
            }
            Ok(code as i32)
        }
        client::AttachResult::IoError => {
            if !opts.quiet {
                eprintln!("mneme: {}: exited due to I/O errors", opts.session_name);
            }
            Ok(1)
        }
    }
}

fn session_exists(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let path = socket::socket_path(name)?;
    Ok(path.exists())
}

fn list_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let dir = socket::socket_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut sessions: Vec<(String, Option<protocol::Welcome>)> = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip non-sockets
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        // Try to query the session
        let welcome = match query_session(&path) {
            Ok(w) => Some(w),
            Err(_) => {
                // Stale socket — try to clean up
                let _ = std::fs::remove_file(&path);
                None
            }
        };

        if let Some(ref w) = welcome {
            sessions.push((name, Some(w.clone())));
        }
    }

    if sessions.is_empty() {
        return Ok(());
    }

    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    println!("Active sessions");
    for (name, welcome) in &sessions {
        if let Some(w) = welcome {
            let status = if !w.child_running {
                '+'
            } else if w.client_count > 0 {
                '*'
            } else {
                ' '
            };
            println!(
                "{status} {pid:>6}  {name}",
                pid = w.server_pid,
                name = name,
            );
        }
    }

    Ok(())
}

fn query_session(
    path: &std::path::Path,
) -> Result<protocol::Welcome, Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;

    let hello = protocol::Hello {
        version: protocol::PROTOCOL_VERSION,
        intent: protocol::Intent::Query,
        flags: protocol::ClientFlags::empty(),
        rows: 0,
        cols: 0,
    };
    protocol::send_packet(stream.as_fd(), &protocol::Packet::hello(&hello))?;

    let pkt = protocol::recv_packet(stream.as_fd())?;
    match pkt.msg_type {
        protocol::MsgType::Welcome => pkt
            .parse_welcome()
            .ok_or_else(|| "malformed welcome".into()),
        protocol::MsgType::Error => {
            let msg = pkt.parse_error().unwrap_or_else(|| "unknown error".into());
            Err(msg.into())
        }
        _ => Err("unexpected response".into()),
    }
}
