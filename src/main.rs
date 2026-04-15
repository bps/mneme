mod client;
mod protocol;
mod ring;
mod server;
mod socket;

use clap::Parser;
use std::env;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Internal server mode — not user-facing, not handled by clap
    if args.get(1).map(|s| s.as_str()) == Some("--server") {
        match server::run_server(&args[2..]) {
            Ok(()) => process::exit(0),
            Err(e) => {
                eprintln!("mneme: server error: {e}");
                process::exit(1);
            }
        }
    }

    let cli = Cli::parse();

    match run_cli(cli) {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("mn: {e}");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Session persistence for terminal processes.
///
/// Run with no arguments to list active sessions.
#[derive(Parser, Debug)]
#[command(name = "mn", version, about)]
struct Cli {
    /// Create session and attach
    #[arg(short = 'c', group = "action")]
    create: bool,

    /// Create session without attaching
    #[arg(short = 'n', group = "action")]
    create_only: bool,

    /// Attach to existing session
    #[arg(short = 'a', group = "action")]
    attach: bool,

    /// Attach if session exists, otherwise create
    #[arg(short = 'A', group = "action")]
    attach_or_create: bool,

    /// Detach key (e.g. ^q for Ctrl-Q)
    #[arg(short = 'e', value_name = "KEY", value_parser = parse_detach_key, default_value = "^\x5c")]
    detach_key: u8,

    /// Attach in read-only mode
    #[arg(short = 'r')]
    readonly: bool,

    /// Low-priority client (defer resize to others)
    #[arg(short = 'l')]
    low_priority: bool,

    /// Suppress informational messages
    #[arg(short = 'q')]
    quiet: bool,

    /// Force reuse of existing session name
    #[arg(short = 'f')]
    force: bool,

    /// Ring buffer size (e.g. 2M, 512K, 65536)
    #[arg(short = 's', value_name = "SIZE", value_parser = parse_size, default_value = "1M")]
    ring_size: usize,

    /// Session name
    #[arg(value_name = "NAME")]
    session_name: Option<String>,

    /// Command to run in the session
    #[arg(trailing_var_arg = true, value_name = "CMD")]
    command: Vec<String>,
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
// Dispatch
// ---------------------------------------------------------------------------

/// Resolve which action to take from the parsed CLI flags.
fn resolve_action(cli: &Cli) -> Result<Action, String> {
    let needs_name = cli.create || cli.create_only || cli.attach || cli.attach_or_create;
    if needs_name && cli.session_name.is_none() {
        return Err("session name required".into());
    }
    if cli.create {
        Ok(Action::Create)
    } else if cli.create_only {
        Ok(Action::CreateOnly)
    } else if cli.attach {
        Ok(Action::Attach)
    } else if cli.attach_or_create {
        Ok(Action::AttachOrCreate)
    } else if cli.session_name.is_none() {
        Ok(Action::List)
    } else {
        Err("an action flag (-c, -n, -a, or -A) is required when a session name is given".into())
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    List,
    Create,
    CreateOnly,
    Attach,
    AttachOrCreate,
}

fn run_cli(cli: Cli) -> Result<i32, Box<dyn std::error::Error>> {
    let action = resolve_action(&cli)?;

    // Validate session name for actions that need one
    if let Some(ref name) = cli.session_name {
        socket::validate_session_name(name)?;
    }

    match action {
        Action::List => {
            list_sessions()?;
            Ok(0)
        }
        Action::Create => {
            create_session(&cli)?;
            attach_session(&cli)
        }
        Action::CreateOnly => {
            create_session(&cli)?;
            Ok(0)
        }
        Action::Attach => attach_session(&cli),
        Action::AttachOrCreate => {
            let name = cli.session_name.as_deref().unwrap();
            if session_exists(name)? {
                attach_session(&cli)
            } else {
                create_session(&cli)?;
                attach_session(&cli)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session operations
// ---------------------------------------------------------------------------

fn session_name(cli: &Cli) -> &str {
    cli.session_name.as_deref().expect("session name required")
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

fn create_session(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let name = session_name(cli);
    let socket_path = socket::socket_path(name)?;

    // Check for existing session
    if socket_path.exists() {
        if !cli.force {
            match std::os::unix::net::UnixStream::connect(&socket_path) {
                Ok(_) => {
                    return Err(format!(
                        "session '{name}' already exists (use -f to force)"
                    )
                    .into());
                }
                Err(_) => {
                    return Err(format!(
                        "stale socket for '{name}' exists (use -f to force)"
                    )
                    .into());
                }
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    let (rows, cols) = get_terminal_size();
    let cmd = if cli.command.is_empty() {
        default_command()
    } else {
        cli.command.clone()
    };

    // Create a pipe for readiness signaling.
    // Use libc directly to avoid CLOEXEC on the write end —
    // the server (child) needs the write end to survive exec.
    let (cli_fd, server_raw_fd) = {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        (unsafe { OwnedFd::from_raw_fd(fds[0]) }, fds[1])
    };

    let exe = env::current_exe()?;
    let mut server_args: Vec<String> = vec![
        "--server".into(),
        name.into(),
        "--ready-fd".into(),
        format!("{server_raw_fd}"),
        "--rows".into(),
        format!("{rows}"),
        "--cols".into(),
        format!("{cols}"),
        "--ring-size".into(),
        format!("{}", cli.ring_size),
        "--socket-path".into(),
        socket_path.to_string_lossy().to_string(),
        "--".into(),
    ];
    server_args.extend(cmd);

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

    unsafe {
        libc::close(server_raw_fd);
    }

    // Wait for readiness: EOF = ready, data = error
    let mut error_buf = vec![0u8; 4096];
    let n = rustix::io::read(&cli_fd, &mut error_buf)?;
    if n > 0 {
        let msg = String::from_utf8_lossy(&error_buf[..n]);
        return Err(format!("server startup failed: {msg}").into());
    }

    if !cli.quiet {
        eprintln!("mn: {name}: session created");
    }
    Ok(())
}

fn attach_session(cli: &Cli) -> Result<i32, Box<dyn std::error::Error>> {
    let name = session_name(cli);
    let socket_path = socket::socket_path(name)?;
    let flags = {
        let mut f = protocol::ClientFlags::empty();
        if cli.readonly {
            f |= protocol::ClientFlags::READONLY;
        }
        if cli.low_priority {
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
        cli.detach_key,
        cli.quiet,
    )?;

    match status {
        client::AttachResult::Detached => {
            if !cli.quiet {
                eprintln!("mn: {name}: detached");
            }
            Ok(0)
        }
        client::AttachResult::Exited(code) => {
            if !cli.quiet {
                eprintln!("mn: {name}: session terminated with exit status {code}");
            }
            Ok(code as i32)
        }
        client::AttachResult::IoError => {
            if !cli.quiet {
                eprintln!("mn: {name}: exited due to I/O errors");
            }
            Ok(1)
        }
    }
}

fn session_exists(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let path = socket::socket_path(name)?;
    if !path.exists() {
        return Ok(false);
    }
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => Ok(true),
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            Ok(false)
        }
    }
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

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        let welcome = match query_session(&path) {
            Ok(w) => Some(w),
            Err(_) => {
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
