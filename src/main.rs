//! herdr-stats: live system stats in the corner of herdr's tab bar.

mod config;
mod daemon;
mod keys;
mod monitor;
mod preview;
mod render;
mod sample;
mod setup;
mod shared;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "\
herdr-stats — live system stats in the corner of herdr's tab bar

usage: herdr-stats <command> [options]

  setup [--interval N] [--preset P]   add the stats to herdr's tab bar and reload
        [--no-reload]                 (interval 1-60 seconds; presets: minimal,
                                      compact, balanced, full)
  setup --remove [--no-reload]        take them out again
  preview [--presets] [--demo]        mock tab bar in this terminal, live
          [--preset P] [--seconds N]
  doctor [--wait]                     sensors, timings, daemon and setup state
  config [--edit]                     print (or edit) the config file path
  open <preview|doctor|config>        open that popup in herdr
  line [--preset P]                   render one line in-process and print it
  status | start | stop | restart     manage the background daemon
  daemon [--foreground]               run the daemon (normally started for you)
  version                             print the version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[args.len().min(1)..];
    let code = match command {
        "daemon" => daemon::run(flag(rest, "--foreground")),
        "start" => start(),
        "stop" => stop(),
        "restart" => {
            stop();
            start()
        }
        "status" => status(),
        "line" => line(option(rest, "--preset")),
        "preview" => preview::run(preview::Options {
            presets: flag(rest, "--presets"),
            demo: flag(rest, "--demo"),
            seconds: option(rest, "--seconds").and_then(|s| s.parse().ok()),
            preset: option(rest, "--preset"),
        }),
        "doctor" => {
            let code = doctor();
            if flag(rest, "--wait") {
                print!("\n  press any key to close");
                let _ = std::io::stdout().flush();
                keys::wait_for_any();
            }
            code
        }
        "config" => edit_config(flag(rest, "--edit")),
        "open" => match rest.first().map(String::as_str) {
            Some(pane @ ("preview" | "doctor" | "config")) => setup::open_pane(pane),
            _ => {
                eprintln!("herdr-stats: open takes preview, doctor or config");
                2
            }
        },
        "setup" if flag(rest, "--remove") => report(setup::remove(!flag(rest, "--no-reload"))),
        "setup" => match option(rest, "--interval")
            .map(|s| s.parse::<u64>())
            .transpose()
        {
            Ok(interval) => report(setup::install(&setup::Options {
                interval,
                preset: option(rest, "--preset"),
                reload: !flag(rest, "--no-reload"),
            })),
            Err(_) => {
                eprintln!("herdr-stats: --interval takes a whole number of seconds, 1 to 60");
                2
            }
        },
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            0
        }
        "-V" | "--version" | "version" => {
            println!("herdr-stats {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("herdr-stats: unknown command `{other}`\n\n{USAGE}");
            2
        }
    };
    ExitCode::from(code.clamp(0, 255) as u8)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn option(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == name {
            args.get(i + 1).cloned()
        } else {
            arg.strip_prefix(&prefix).map(str::to_string)
        }
    })
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(message) => {
            eprintln!("herdr-stats: {message}");
            1
        }
    }
}

fn ask(argument: &str) -> Option<String> {
    let mut stream = UnixStream::connect(shared::socket_path()).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    writeln!(stream, "{} {argument}", shared::PROTOCOL).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply.trim_end().to_string())
}

fn spawn_daemon() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().is_ok()
}

/// Startup hook: re-point the stable bar link at this install, seed the config
/// file, and make sure a daemon is answering. Idempotent.
fn start() -> i32 {
    if let Err(err) = setup::refresh_bar_link() {
        eprintln!("herdr-stats: {err}");
    }
    seed_config();
    if ask(shared::STATUS_REQUEST).is_some() {
        return 0;
    }
    if !spawn_daemon() {
        eprintln!("herdr-stats: could not start the daemon");
        return 1;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if ask(shared::STATUS_REQUEST).is_some() {
            return 0;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "herdr-stats: daemon did not come up; see {}",
        shared::log_path().display()
    );
    1
}

fn stop() -> i32 {
    if ask(shared::STOP_REQUEST).is_none() {
        return 0;
    }
    // Wait for the socket to go so a following start does not race it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && UnixStream::connect(shared::socket_path()).is_ok() {
        std::thread::sleep(Duration::from_millis(20));
    }
    0
}

fn status() -> i32 {
    match ask(shared::STATUS_REQUEST) {
        Some(status) => {
            println!("running  {status}");
            0
        }
        None => {
            println!("not running (herdr starts it on the next tab bar tick)");
            if let Some(pid) = daemon::read_pid() {
                println!("last pid {pid}");
            }
            3
        }
    }
}

fn line(preset: Option<String>) -> i32 {
    let mut monitor = monitor::Monitor::new();
    let preset = preset.unwrap_or_default();
    monitor.frame(&preset);
    std::thread::sleep(Duration::from_millis(1000));
    println!("{}", monitor.frame(&preset));
    0
}

fn seed_config() {
    let path = config::path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, config::TEMPLATE);
    }
}

fn edit_config(edit: bool) -> i32 {
    seed_config();
    let path = config::path();
    if !edit {
        println!("{}", path.display());
        return 0;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    i32::from(!status.is_ok_and(|s| s.success()))
}

fn doctor() -> i32 {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "herdr-stats {}", env!("CARGO_PKG_VERSION"));
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let row = |out: &mut std::io::StdoutLock, key: &str, value: String| {
        let _ = writeln!(out, "  {key:<10} {value}");
    };
    row(
        &mut out,
        "binary",
        exe.map_or("?".into(), |p| p.display().to_string()),
    );
    row(&mut out, "state", shared::state_dir().display().to_string());
    let config_path = config::path();
    let config_state = match config::load(&config_path) {
        Ok(file) => match config::resolve(&file, None) {
            Ok(layout) => format!("ok, preset {}", layout.preset),
            Err(err) => format!("error: {err}"),
        },
        Err(err) => format!("error: {err}"),
    };
    row(
        &mut out,
        "config",
        format!("{} ({config_state})", config_path.display()),
    );
    row(
        &mut out,
        "daemon",
        ask(shared::STATUS_REQUEST).unwrap_or_else(|| "not running".into()),
    );
    row(
        &mut out,
        "tab bar",
        match setup::installed_command() {
            Some(entry) => format!(
                "installed in {}\n             {entry}",
                setup::herdr_config_path().display()
            ),
            None => "not installed; run `herdr-stats setup`".into(),
        },
    );

    let _ = writeln!(out, "\nsensors");
    let mut monitor = monitor::Monitor::new();
    for line in monitor.probe().describe() {
        let _ = writeln!(out, "  {line}");
    }

    let _ = writeln!(out, "\ncost per read (median of 15)");
    let probe = monitor.probe();
    let mut reading = sample::Reading::default();
    let mut time = |name: &str, f: &mut dyn FnMut(&mut sample::Probe, &mut sample::Reading)| {
        let mut runs: Vec<Duration> = (0..15)
            .map(|_| {
                let started = Instant::now();
                f(probe, &mut reading);
                started.elapsed()
            })
            .collect();
        runs.sort();
        let _ = writeln!(
            out,
            "  {name:<9} {:>7.1} µs",
            runs[runs.len() / 2].as_secs_f64() * 1e6
        );
    };
    time("cpu", &mut |p, r| p.cpu(r));
    time("memory", &mut |p, r| p.memory(r));
    time("gpu", &mut |p, r| p.gpu(r));
    time("network", &mut |p, r| p.network(r));
    let hottest = sample::TempWant {
        cpu: true,
        gpu: true,
        soc: false,
    };
    time("thermal", &mut |p, r| p.thermal(r, hottest));
    time("power", &mut |p, r| p.power(r, true, true));
    time("battery", &mut |p, r| p.battery(r));
    time("disk", &mut |p, r| p.disk(r, "/"));

    let _ = writeln!(out, "\nreading");
    let _ = writeln!(out, "  {:?}", reading);
    let _ = writeln!(out, "\nline (every preset)");
    std::thread::sleep(Duration::from_millis(900));
    for preset in config::PRESETS {
        let frame = monitor.frame(preset);
        let _ = writeln!(
            out,
            "  {preset:<9} {frame}   [{} cells]",
            render::cells(&frame)
        );
    }
    0
}
