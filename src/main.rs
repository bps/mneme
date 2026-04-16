mod client;
mod protocol;
mod ring;
mod server;
mod socket;

use clap::{Parser, Subcommand};
use std::env;
use std::os::fd::{AsFd, AsRawFd};
use std::process;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Message(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<String> for Error {
    fn from(msg: String) -> Self {
        Error::Message(msg)
    }
}

impl From<&str> for Error {
    fn from(msg: &str) -> Self {
        Error::Message(msg.to_owned())
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Error::Io(e.into())
    }
}

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
#[derive(Parser, Debug)]
#[command(name = "mn", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a new session and attach to it.
    #[command(alias = "c")]
    Create(CreateOpts),

    /// Create a new session without attaching.
    #[command(alias = "n")]
    New(NewOpts),

    /// Attach to an existing session.
    #[command(alias = "a")]
    Attach(AttachOpts),

    /// Attach to a session if it exists, otherwise create it.
    #[command(alias = "A")]
    Auto(AutoOpts),

    /// List active sessions.
    #[command(alias = "ls")]
    List,

    /// Kill a session (terminates the server and child process).
    #[command(alias = "rm")]
    Kill(KillOpts),
}

// -- Shared option groups ---------------------------------------------------

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
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024)
    } else {
        (s, 1)
    };
    let n: usize = num_str.parse().map_err(|_| format!("invalid size: {s}"))?;
    Ok(n * multiplier)
}

#[derive(Parser, Debug)]
struct CreateOpts {
    /// Session name.
    name: String,

    /// Command to run in the session.
    #[arg(trailing_var_arg = true, value_name = "CMD")]
    command: Vec<String>,

    /// Detach key (e.g. ^q for Ctrl-Q).
    #[arg(short = 'e', value_name = "KEY", value_parser = parse_detach_key, default_value = "^\x5c")]
    detach_key: u8,

    /// Ring buffer size (e.g. 2M, 512K, 65536).
    #[arg(short = 's', value_name = "SIZE", value_parser = parse_size, default_value = "1M")]
    ring_size: usize,

    /// Force reuse of existing session name.
    #[arg(short = 'f')]
    force: bool,

    /// Suppress informational messages.
    #[arg(short = 'q')]
    quiet: bool,
}

#[derive(Parser, Debug)]
struct NewOpts {
    /// Session name.
    name: String,

    /// Command to run in the session.
    #[arg(trailing_var_arg = true, value_name = "CMD")]
    command: Vec<String>,

    /// Ring buffer size (e.g. 2M, 512K, 65536).
    #[arg(short = 's', value_name = "SIZE", value_parser = parse_size, default_value = "1M")]
    ring_size: usize,

    /// Force reuse of existing session name.
    #[arg(short = 'f')]
    force: bool,

    /// Suppress informational messages.
    #[arg(short = 'q')]
    quiet: bool,
}

#[derive(Parser, Debug)]
struct AttachOpts {
    /// Session name.
    name: String,

    /// Detach key (e.g. ^q for Ctrl-Q).
    #[arg(short = 'e', value_name = "KEY", value_parser = parse_detach_key, default_value = "^\x5c")]
    detach_key: u8,

    /// Attach in read-only mode.
    #[arg(short = 'r')]
    readonly: bool,

    /// Low-priority client (defer resize to others).
    #[arg(short = 'l')]
    low_priority: bool,

    /// Suppress informational messages.
    #[arg(short = 'q')]
    quiet: bool,
}

#[derive(Parser, Debug)]
struct KillOpts {
    /// Session name (or "all" to kill every session).
    name: String,

    /// Suppress informational messages.
    #[arg(short = 'q')]
    quiet: bool,
}

#[derive(Parser, Debug)]
struct AutoOpts {
    /// Session name.
    name: String,

    /// Command to run if creating a new session.
    #[arg(trailing_var_arg = true, value_name = "CMD")]
    command: Vec<String>,

    /// Detach key (e.g. ^q for Ctrl-Q).
    #[arg(short = 'e', value_name = "KEY", value_parser = parse_detach_key, default_value = "^\x5c")]
    detach_key: u8,

    /// Ring buffer size (e.g. 2M, 512K, 65536).
    #[arg(short = 's', value_name = "SIZE", value_parser = parse_size, default_value = "1M")]
    ring_size: usize,

    /// Attach in read-only mode.
    #[arg(short = 'r')]
    readonly: bool,

    /// Low-priority client (defer resize to others).
    #[arg(short = 'l')]
    low_priority: bool,

    /// Force reuse of existing session name.
    #[arg(short = 'f')]
    force: bool,

    /// Suppress informational messages.
    #[arg(short = 'q')]
    quiet: bool,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn run_cli(cli: Cli) -> Result<i32, Error> {
    match cli.command {
        None | Some(Cmd::List) => {
            list_sessions()?;
            Ok(0)
        }
        Some(Cmd::Create(opts)) => {
            socket::validate_session_name(&opts.name)?;
            do_create(
                &opts.name,
                &opts.command,
                opts.ring_size,
                opts.force,
                opts.quiet,
            )?;
            do_attach(&opts.name, opts.detach_key, false, false, opts.quiet)
        }
        Some(Cmd::New(opts)) => {
            socket::validate_session_name(&opts.name)?;
            do_create(
                &opts.name,
                &opts.command,
                opts.ring_size,
                opts.force,
                opts.quiet,
            )?;
            Ok(0)
        }
        Some(Cmd::Attach(opts)) => {
            socket::validate_session_name(&opts.name)?;
            do_attach(
                &opts.name,
                opts.detach_key,
                opts.readonly,
                opts.low_priority,
                opts.quiet,
            )
        }
        Some(Cmd::Auto(opts)) => {
            socket::validate_session_name(&opts.name)?;
            if session_exists(&opts.name)? {
                do_attach(
                    &opts.name,
                    opts.detach_key,
                    opts.readonly,
                    opts.low_priority,
                    opts.quiet,
                )
            } else {
                do_create(
                    &opts.name,
                    &opts.command,
                    opts.ring_size,
                    opts.force,
                    opts.quiet,
                )?;
                do_attach(
                    &opts.name,
                    opts.detach_key,
                    opts.readonly,
                    opts.low_priority,
                    opts.quiet,
                )
            }
        }
        Some(Cmd::Kill(opts)) => {
            if opts.name == "all" {
                do_kill_all(opts.quiet)
            } else {
                socket::validate_session_name(&opts.name)?;
                do_kill(&opts.name, opts.quiet)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session operations
// ---------------------------------------------------------------------------

fn default_command() -> Vec<String> {
    if let Ok(cmd) = env::var("MNEME_CMD") {
        return vec!["/bin/sh".into(), "-c".into(), cmd];
    }
    if let Ok(shell) = env::var("SHELL") {
        return vec![shell];
    }
    vec!["/bin/sh".into()]
}

pub fn get_terminal_size() -> (u16, u16) {
    use rustix::termios::tcgetwinsize;
    match tcgetwinsize(std::io::stdin()) {
        Ok(ws) => (ws.ws_row, ws.ws_col),
        Err(_) => (24, 80),
    }
}

fn do_create(
    name: &str,
    command: &[String],
    ring_size: usize,
    force: bool,
    quiet: bool,
) -> Result<(), Error> {
    let socket_path = socket::socket_path(name)?;
    let lock_path = socket::lock_path(name)?;

    // Liveness check: a session is alive iff a process holds its lock.
    // The kernel releases flock on any form of process death, so this
    // correctly identifies crashed servers without racy sleeps or
    // PID-reuse hazards.
    if socket_path.exists() || lock_path.exists() {
        let stale = socket::is_session_stale(&lock_path);
        if !stale && !force {
            return Err(format!("session '{name}' already exists (use -f to force)").into());
        }
        // Either stale (safe to clean) or forced. In either case, clean
        // up both files before the server tries to bind.
        socket::cleanup_stale_session(&socket_path, &lock_path);
    }

    let (rows, cols) = get_terminal_size();
    let cmd = if command.is_empty() {
        default_command()
    } else {
        command.to_vec()
    };

    let (cli_fd, server_fd) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&cli_fd, rustix::io::FdFlags::CLOEXEC)?;

    let exe = env::current_exe()?;
    let server_raw_fd = server_fd.as_raw_fd();
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
        format!("{ring_size}"),
        "--socket-path".into(),
        socket_path.to_string_lossy().into_owned(),
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

    drop(server_fd);

    let mut error_buf = vec![0u8; 4096];
    let n = rustix::io::read(&cli_fd, &mut error_buf)?;
    if n > 0 {
        let msg = String::from_utf8_lossy(&error_buf[..n]);
        return Err(format!("server startup failed: {msg}").into());
    }

    if !quiet {
        eprintln!("mn: {name}: session created");
    }
    Ok(())
}

fn do_attach(
    name: &str,
    detach_key: u8,
    readonly: bool,
    low_priority: bool,
    quiet: bool,
) -> Result<i32, Error> {
    let socket_path = socket::socket_path(name)?;
    let flags = {
        let mut f = protocol::ClientFlags::empty();
        if readonly {
            f |= protocol::ClientFlags::READONLY;
        }
        if low_priority {
            f |= protocol::ClientFlags::LOW_PRIORITY;
        }
        f
    };
    let (rows, cols) = get_terminal_size();

    let status = client::attach(&socket_path, flags, rows, cols, detach_key, quiet)?;

    match status {
        client::AttachResult::Detached => {
            if !quiet {
                eprintln!("mn: {name}: detached");
            }
            Ok(0)
        }
        client::AttachResult::Exited(code) => {
            if !quiet {
                eprintln!("mn: {name}: session terminated with exit status {code}");
            }
            Ok(code as i32)
        }
        client::AttachResult::IoError => {
            if !quiet {
                eprintln!("mn: {name}: exited due to I/O errors");
            }
            Ok(1)
        }
    }
}

fn session_exists(name: &str) -> Result<bool, Error> {
    let path = socket::socket_path(name)?;
    let lock_path = socket::lock_path(name)?;
    if !path.exists() && !lock_path.exists() {
        return Ok(false);
    }
    if socket::is_session_stale(&lock_path) {
        socket::cleanup_stale_session(&path, &lock_path);
        return Ok(false);
    }
    Ok(true)
}

fn do_kill(name: &str, quiet: bool) -> Result<i32, Error> {
    let path = socket::socket_path(name)?;
    let lock_path = socket::lock_path(name)?;
    let welcome = match query_session(&path) {
        Ok(w) => w,
        Err(_) => {
            // Socket/lock exists but server isn't responding. If the lock
            // can be acquired, the server is truly dead — clean up.
            if path.exists() || lock_path.exists() {
                if socket::is_session_stale(&lock_path) {
                    socket::cleanup_stale_session(&path, &lock_path);
                    if !quiet {
                        eprintln!("mn: {name}: removed stale session");
                    }
                    return Ok(0);
                }
                return Err(format!("session '{name}' not responding but lock still held").into());
            }
            return Err(format!("session '{name}' not found").into());
        }
    };

    // Kill the server process
    let pid = rustix::process::Pid::from_raw(welcome.server_pid as i32)
        .ok_or_else(|| Error::from(format!("invalid server pid: {}", welcome.server_pid)))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).map_err(|e| {
        Error::from(format!(
            "failed to kill server (pid {}): {e}",
            welcome.server_pid
        ))
    })?;

    // Wait for the server to actually die by polling the lock. The kernel
    // releases flock on process death, so once we can acquire the lock,
    // the server is gone. Bounded to ~2s as a safety net.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if socket::is_session_stale(&lock_path) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                format!("server (pid {}) did not exit within 2s", welcome.server_pid).into(),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Remove any leftover files (the server normally cleans these up itself).
    socket::cleanup_stale_session(&path, &lock_path);

    if !quiet {
        eprintln!("mn: {name}: killed");
    }
    Ok(0)
}

fn do_kill_all(quiet: bool) -> Result<i32, Error> {
    let dir = socket::socket_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        use std::os::unix::fs::FileTypeExt;
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_socket() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        match do_kill(&name, quiet) {
            Ok(_) => count += 1,
            Err(e) => {
                if !quiet {
                    eprintln!("mn: {name}: {e}");
                }
            }
        }
    }

    // Sweep orphan lock files (no paired socket) whose owning server is
    // gone. This covers servers that died after unlinking the socket but
    // before unlinking the lock.
    if let Ok(entries2) = std::fs::read_dir(&dir) {
        for entry in entries2.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".lock") {
                continue;
            }
            let lock_path = entry.path();
            if socket::is_session_stale(&lock_path) {
                let _ = std::fs::remove_file(&lock_path);
            }
        }
    }

    if !quiet && count == 0 {
        eprintln!("mn: no sessions to kill");
    }
    Ok(0)
}

fn list_sessions() -> Result<(), Error> {
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

        let name = entry.file_name().to_string_lossy().into_owned();

        // Skip lock files; they're paired with the real socket entries.
        if name.ends_with(".lock") {
            continue;
        }

        let lock_path = socket::lock_path(&name)?;
        if socket::is_session_stale(&lock_path) {
            // Server is dead — clean up both files and skip.
            socket::cleanup_stale_session(&path, &lock_path);
            continue;
        }

        // Lock is held but query may fail (e.g. server is in
        // startup or shutdown). Skip rather than unlink —
        // unlinking a live session's socket would be harmful.
        let welcome = query_session(&path).ok();

        if let Some(ref w) = welcome {
            sessions.push((name, Some(w.clone())));
        }
    }

    if sessions.is_empty() {
        return Ok(());
    }

    sessions.sort_unstable_by(|a, b| a.0.cmp(&b.0));
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
            println!("{status} {pid:>6}  {name}", pid = w.server_pid, name = name,);
        }
    }

    Ok(())
}

fn query_session(path: &std::path::Path) -> Result<protocol::Welcome, Error> {
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
