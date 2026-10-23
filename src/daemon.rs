//! The long-lived process behind the tab bar.
//!
//! One daemon per user (per state dir) answers every herdr session on the
//! machine. It blocks in `poll` between requests, so it costs nothing while
//! idle, and exits by itself once herdr stops asking (server stopped, segment
//! removed) or its binary is replaced by a rebuild or plugin update.

use crate::monitor::Monitor;
use crate::shared;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Long enough to ride out a herdr restart or a slow `interval_seconds`.
const IDLE_EXIT: Duration = Duration::from_secs(180);
const EXE_CHECK_EVERY: Duration = Duration::from_secs(5);
/// The whole request, not each read: a client that trickles bytes must not hold
/// up the clients queued behind it.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const LOCK_WAIT: Duration = Duration::from_millis(1500);
/// The longest `poll` sleeps, so a signal that lands just before it is still
/// seen promptly.
const POLL_SLICE: Duration = Duration::from_secs(1);
const MAX_LOG_BYTES: u64 = 256 * 1024;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn run(foreground: bool) -> i32 {
    let state = shared::state_dir();
    if let Err(err) = std::fs::create_dir_all(&state) {
        eprintln!("herdr-stats: cannot create {}: {err}", state.display());
        return 1;
    }
    if !foreground {
        redirect_stderr_to_log();
    }
    let Some(lock) = acquire_lock() else {
        // Another daemon is serving; nothing to do.
        return 0;
    };
    write_pid(&lock);

    let socket = shared::socket_path();
    if let Err(err) = shared::prepare_socket_dir(&socket) {
        log(&format!("cannot use the socket directory: {err}"));
        return 1;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(err) => {
            log(&format!("bind {} failed: {err}", socket.display()));
            return 1;
        }
    };
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    install_signal_handlers();
    log(&format!(
        "started pid {} on {}",
        std::process::id(),
        socket.display()
    ));

    let exe = std::env::current_exe().ok();
    let exe_stamp = exe.as_deref().and_then(identity);
    let mut exe_checked = Instant::now();
    let started = Instant::now();
    let mut last_request = Instant::now();
    let mut served: u64 = 0;
    let mut monitor = Monitor::new();

    let reason = loop {
        if STOP.load(Ordering::SeqCst) {
            break "signal";
        }
        let idle_left = IDLE_EXIT.saturating_sub(last_request.elapsed());
        if idle_left.is_zero() {
            break "idle";
        }
        if !wait_readable(&listener, idle_left.min(POLL_SLICE)) {
            continue;
        }
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        last_request = Instant::now();
        served += 1;
        match serve(stream, &mut monitor, started, served) {
            Served::Frame | Served::Ignored => {}
            Served::Stop => break "stop request",
            Served::Outdated => break "client speaks a newer protocol",
        }
        if exe_checked.elapsed() >= EXE_CHECK_EVERY {
            exe_checked = Instant::now();
            if exe_stamp.is_some() && exe.as_deref().and_then(identity) != exe_stamp {
                break "binary replaced";
            }
        }
    };

    let _ = std::fs::remove_file(&socket);
    log(&format!("exiting ({reason}) after {served} requests"));
    drop(lock);
    0
}

enum Served {
    Frame,
    Stop,
    Outdated,
    /// Empty or not ours: closed without an answer, and the daemon carries on.
    Ignored,
}

fn serve(mut stream: UnixStream, monitor: &mut Monitor, started: Instant, served: u64) -> Served {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let mut request = [0u8; 256];
    let mut len = 0;
    while len < request.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match stream.read(&mut request[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if request[..len].contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&request[..len]);
    let mut words = text.split_ascii_whitespace();
    let protocol = words.next().unwrap_or("");
    let argument = words.next().unwrap_or("");

    if protocol != shared::PROTOCOL {
        // Only a client of a newer release (after an update, before this
        // daemon noticed its binary changed) makes way for a new daemon.
        // Anything else, such as a probe that connects and sends nothing,
        // must not take the tab bar down.
        let newer = protocol
            .strip_prefix("hs")
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n > protocol_version());
        if newer {
            let _ = stream.write_all(b"!restart\n");
            return Served::Outdated;
        }
        return Served::Ignored;
    }
    match argument {
        shared::STOP_REQUEST => {
            let _ = stream.write_all(b"ok\n");
            Served::Stop
        }
        shared::STATUS_REQUEST => {
            let mut status = format!(
                "pid={} uptime={}s served={} sample_us={} config_error={}",
                std::process::id(),
                started.elapsed().as_secs(),
                served,
                monitor.sample_cost.as_micros(),
                monitor.config_error.as_deref().unwrap_or("none"),
            );
            if let Some(preset) = &monitor.unknown_preset {
                status.push_str(&format!(" unknown_preset={preset}"));
            }
            status.push('\n');
            let _ = stream.write_all(status.as_bytes());
            Served::Frame
        }
        preset => {
            let mut frame = monitor.frame(preset);
            frame.push('\n');
            let _ = stream.write_all(frame.as_bytes());
            Served::Frame
        }
    }
}

fn wait_readable(listener: &UnixListener, timeout: Duration) -> bool {
    let mut fds = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut fds, 1, millis) };
    ready > 0 && fds.revents & libc::POLLIN != 0
}

fn protocol_version() -> u32 {
    shared::PROTOCOL
        .trim_start_matches("hs")
        .parse()
        .unwrap_or(1)
}

fn acquire_lock() -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(shared::lock_path())
        .ok()?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(file);
        }
        // A daemon told to restart may still be on its way out.
        if daemon_answers() {
            return None;
        }
        if Instant::now() >= deadline {
            // The holder is alive (flock dies with its process) but has not
            // answered for the whole wait: stopped or stuck in a sensor call.
            // Take over, or the tab bar stays blank until someone kills it.
            if !kill_unresponsive_holder() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
            return (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0)
                .then_some(file);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether a daemon is serving: a real status request, since a socket that
/// accepts connections may belong to a daemon that is wedged.
fn daemon_answers() -> bool {
    let Ok(mut stream) = UnixStream::connect(shared::socket_path()) else {
        return false;
    };
    let timeout = Some(Duration::from_millis(300));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return false;
    }
    let request = format!("{} {}\n", shared::PROTOCOL, shared::STATUS_REQUEST);
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).is_ok() && &reply == b"pid="
}

/// SIGKILL the daemon holding the lock, but only if the pid in the lock file
/// is a herdr-stats process of ours.
fn kill_unresponsive_holder() -> bool {
    let Some(pid) = read_pid() else { return false };
    if pid <= 1 || pid == std::process::id() as i32 || !is_herdr_stats(pid) {
        return false;
    }
    log(&format!(
        "daemon {pid} holds the lock but does not answer; killing it"
    ));
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

#[cfg(target_os = "macos")]
fn is_herdr_stats(pid: i32) -> bool {
    let mut path = [0u8; 4096];
    let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    len > 0 && path[..len as usize].ends_with(b"/herdr-stats")
}

#[cfg(not(target_os = "macos"))]
fn is_herdr_stats(pid: i32) -> bool {
    // A binary replaced on disk reads as "herdr-stats (deleted)".
    std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| {
        exe.file_name()
            .is_some_and(|name| name == "herdr-stats" || name == "herdr-stats (deleted)")
    })
}

fn write_pid(lock: &File) {
    let mut lock = lock;
    let _ = lock.set_len(0);
    let _ = writeln!(lock, "{}", std::process::id());
}

pub fn read_pid() -> Option<i32> {
    std::fs::read_to_string(shared::lock_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn install_signal_handlers() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        // No SA_RESTART: poll should return so the loop sees STOP.
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// What identifies the binary on disk: a rebuild or update replaces the file
/// (new inode) or rewrites it (new mtime); either one means exit.
fn identity(path: &Path) -> Option<(u64, u64, i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino(), meta.mtime(), meta.mtime_nsec()))
}

fn redirect_stderr_to_log() {
    let path = shared::log_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    }
}

pub fn log(message: &str) {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    eprintln!("[{secs}] {message}");
}
