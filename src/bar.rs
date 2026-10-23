//! `herdr-stats-bar`: the command herdr's tab bar runs every second.
//!
//! It asks the daemon for the current line over a Unix socket and prints it.
//! All sampling and rendering lives in the daemon, so this binary stays tiny
//! and links nothing heavier than libSystem; on every tick herdr pays for a
//! login shell, and this adds well under a millisecond on top.
//!
//! If the daemon is not running (first tick after login, after an upgrade, or
//! after it exited because herdr stopped asking) this starts it and retries, so
//! the tab bar heals itself without a startup hook.

#[allow(dead_code)]
mod shared;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// herdr gives up on the command after `timeout_seconds` (2 s by default);
/// everything here, including starting a daemon, fits well inside that.
const BUDGET: Duration = Duration::from_millis(1500);
/// One request to a warm daemon takes well under a millisecond.
const IO_TIMEOUT: Duration = Duration::from_millis(700);
const RESTART_REPLY: &str = "!restart";

fn main() -> ExitCode {
    if matches!(std::env::args().nth(1).as_deref(), Some("-V" | "--version")) {
        println!("herdr-stats-bar {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let deadline = Instant::now() + BUDGET;
    let preset = parse_preset();
    let request = format!("{} {}\n", shared::PROTOCOL, preset.as_deref().unwrap_or(""));

    let reply = match ask(&request, deadline) {
        Ok(reply) if reply.trim_end() == RESTART_REPLY => retry_after_spawn(&request, deadline),
        Ok(reply) => Some(reply),
        Err(_) => retry_after_spawn(&request, deadline),
    };

    match reply {
        Some(line) if !line.trim().is_empty() => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(line.as_bytes());
            if !line.ends_with('\n') {
                let _ = out.write_all(b"\n");
            }
            ExitCode::SUCCESS
        }
        // Printing nothing makes herdr hide the segment, which beats showing a
        // stale or broken reading.
        _ => ExitCode::FAILURE,
    }
}

fn parse_preset() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--preset" || arg == "-p" {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--preset=") {
            return Some(value.to_string());
        }
        if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn ask(request: &str, deadline: Instant) -> std::io::Result<String> {
    let left = deadline
        .saturating_duration_since(Instant::now())
        .min(IO_TIMEOUT);
    if left.is_zero() {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    let mut stream = UnixStream::connect(shared::socket_path())?;
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    stream.write_all(request.as_bytes())?;
    let mut reply = String::with_capacity(512);
    stream.take(8192).read_to_string(&mut reply)?;
    Ok(reply)
}

fn retry_after_spawn(request: &str, deadline: Instant) -> Option<String> {
    spawn_daemon()?;
    let mut pause = Duration::from_millis(5);
    while Instant::now() < deadline {
        std::thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
        pause = (pause * 2).min(Duration::from_millis(60));
        if let Ok(reply) = ask(request, deadline) {
            if reply.trim_end() != RESTART_REPLY {
                return Some(reply);
            }
        }
    }
    None
}

fn spawn_daemon() -> Option<()> {
    // herdr may call us through a stable symlink in the state dir; the daemon
    // binary sits next to the real file, not the link.
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let daemon = exe.with_file_name("herdr-stats");
    let mut command = Command::new(daemon);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // herdr kills the status command's whole process group when it finishes;
    // a new session puts the daemon outside it.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().ok().map(|_| ())
}
