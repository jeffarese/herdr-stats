//! End to end, the way herdr uses it: the first `herdr-stats-bar` call starts a
//! daemon in a private state dir, every call prints one line herdr can show,
//! and the daemon stops on request. On CI this is also the run that exercises
//! each platform's real samplers.

use std::path::PathBuf;
use std::process::{Command, Output};

const BAR: &str = env!("CARGO_BIN_EXE_herdr-stats-bar");
const CLI: &str = env!("CARGO_BIN_EXE_herdr-stats");
const PRESETS: [&str; 4] = ["minimal", "compact", "balanced", "full"];

/// A state and config dir of its own, so the test never talks to a daemon
/// serving a real herdr. Stops its daemon and removes the dirs when dropped.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// `name` keeps tests running in parallel apart.
    fn new(name: &str) -> Self {
        // Short, so the socket path stays well inside sun_path.
        let root = std::env::temp_dir().join(format!("hs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn run(&self, program: &str, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .env("HERDR_PLUGIN_STATE_DIR", self.root.join("state"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.root.join("config"))
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.run(CLI, &["stop"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn line(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(text.ends_with('\n'), "no trailing newline: {text:?}");
    let text = text.trim_end_matches('\n').to_string();
    assert!(!text.contains('\n'), "more than one line: {text:?}");
    assert!(!text.contains('\x1b'), "escape codes in {text:?}");
    assert!(!text.trim().is_empty(), "empty line");
    // herdr keeps the first 80 characters.
    assert!(
        text.chars().count() <= 80,
        "{} chars: {text}",
        text.chars().count()
    );
    text
}

#[test]
fn bar_starts_the_daemon_and_serves_every_preset() {
    let sandbox = Sandbox::new("presets");

    // Cold: no daemon yet, so this call starts one and waits for its answer.
    let first = line(&sandbox.run(BAR, &[]));
    // A number, not the "--%" of a sampler that read nothing.
    assert!(
        first.contains('%') && !first.contains("--%"),
        "no cpu percent in {first}"
    );

    let status = sandbox.run(CLI, &["status"]);
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.starts_with("running"), "status: {status}");

    for preset in PRESETS {
        line(&sandbox.run(BAR, &["--preset", preset]));
    }

    // Stopping removes the socket, and the next call brings a daemon back.
    sandbox.run(CLI, &["stop"]);
    assert!(!sandbox.root.join("state/daemon.sock").exists());
    line(&sandbox.run(BAR, &["compact"]));
}

#[test]
fn stray_connections_do_not_stop_the_daemon_but_a_newer_client_does() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let sandbox = Sandbox::new("stray");
    line(&sandbox.run(BAR, &[]));
    let socket = sandbox.root.join("state/daemon.sock");

    // A probe that connects and says nothing, and one that says nonsense.
    drop(UnixStream::connect(&socket).unwrap());
    let mut garbage = UnixStream::connect(&socket).unwrap();
    garbage.write_all(b"hello there\n").unwrap();
    let mut reply = String::new();
    let _ = garbage.read_to_string(&mut reply);
    assert!(reply.is_empty(), "answered garbage with {reply:?}");
    let status = sandbox.run(CLI, &["status"]);
    assert!(
        String::from_utf8_lossy(&status.stdout).starts_with("running"),
        "daemon died"
    );

    // A client of a newer release tells it to make way.
    let mut newer = UnixStream::connect(&socket).unwrap();
    newer.write_all(b"hs2 \n").unwrap();
    let mut reply = String::new();
    newer.read_to_string(&mut reply).unwrap();
    assert_eq!(reply.trim_end(), "!restart");
}

#[test]
fn bars_starting_together_share_one_daemon() {
    let sandbox = Sandbox::new("together");
    let state = sandbox.root.join("state");
    let config = sandbox.root.join("config");
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(BAR)
                .env("HERDR_PLUGIN_STATE_DIR", &state)
                .env("HERDR_PLUGIN_CONFIG_DIR", &config)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        line(&child.wait_with_output().unwrap());
    }
    let status = sandbox.run(CLI, &["status"]);
    assert!(String::from_utf8_lossy(&status.stdout).starts_with("running"));
}

#[test]
fn version_flags_match_the_package() {
    let expected = env!("CARGO_PKG_VERSION");
    for (program, name) in [(BAR, "herdr-stats-bar"), (CLI, "herdr-stats")] {
        let output = Command::new(program).arg("--version").output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("{name} {expected}")
        );
    }
}
