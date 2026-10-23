//! `herdr-stats preview`: a mock of herdr's tab bar in the current terminal,
//! in the user's herdr theme colours, updating live. Shows every preset side
//! by side with `--presets`, and drives them with synthetic load with `--demo`
//! so the motion is visible on an idle machine.

use crate::config::{self, File};
use crate::monitor::Monitor;
use crate::render::{self, History, View};
use crate::sample::{Battery, Disk, Fan, Mem, Net, Reading, Temps};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

pub struct Options {
    pub presets: bool,
    pub demo: bool,
    pub seconds: Option<u64>,
    pub preset: Option<String>,
}

struct Theme {
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
    dim: (u8, u8, u8),
    text: (u8, u8, u8),
    active_bg: (u8, u8, u8),
}

pub fn run(options: Options) -> i32 {
    if let Some(preset) = options.preset.as_deref() {
        if !config::PRESETS.contains(&preset) {
            eprintln!(
                "herdr-stats: unknown preset `{preset}`; presets: {}",
                config::PRESETS.join(", ")
            );
            return 2;
        }
    }
    let theme = load_theme();
    let presets: Vec<String> = if options.presets {
        config::PRESETS.iter().map(|p| p.to_string()).collect()
    } else {
        vec![options.preset.clone().unwrap_or_default()]
    };
    let mut monitor = Monitor::new();
    let mut demo = Demo::default();
    let file = config::load(&config::path()).unwrap_or_default();

    unsafe {
        libc::signal(
            libc::SIGINT,
            on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let interactive = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    let _raw = interactive.then(crate::keys::RawInput::enable).flatten();
    if _raw.is_some() {
        crate::keys::watch(&QUIT);
    }
    let deadline = options
        .seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut out = std::io::stdout().lock();
    if interactive {
        let _ = write!(out, "\x1b[?25l");
        crate::keys::show_cursor_on_panic();
    }
    let mut drawn = 0usize;
    loop {
        let width = terminal_width();
        let mut rows = Vec::with_capacity(presets.len());
        if options.demo {
            demo.step();
        }
        for preset in &presets {
            let line = if options.demo {
                let layout = config::resolve(&file, Some(preset))
                    .or_else(|_| config::resolve(&File::default(), Some(preset)))
                    .or_else(|_| config::resolve(&File::default(), None));
                layout
                    .map(|layout| render::render(&layout, &demo.view()))
                    .unwrap_or_default()
            } else {
                monitor.frame(preset)
            };
            let label = if options.presets {
                format!("{preset} {}", render::cells(&line))
            } else {
                String::new()
            };
            if options.presets && !rows.is_empty() {
                rows.push(String::new());
            }
            rows.push(tab_bar(&theme, width, &label, &line));
        }
        if interactive {
            let what = if options.demo {
                "synthetic load"
            } else {
                "live"
            };
            rows.push(format!(
                "\x1b[2m  {what} · each row is herdr's tab bar · any key closes\x1b[0m"
            ));
        }
        // The cursor stays on the last row instead of below it, so a terminal
        // exactly as tall as the preview does not scroll on every redraw.
        if interactive && drawn > 1 {
            let _ = write!(out, "\x1b[{}A", drawn - 1);
        }
        for (i, row) in rows.iter().enumerate() {
            let end = if i + 1 < rows.len() || !interactive {
                "\n"
            } else {
                ""
            };
            let _ = write!(out, "\r\x1b[2K{row}{end}");
        }
        let _ = out.flush();
        drawn = rows.len();

        if !interactive || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let next = Instant::now() + Duration::from_secs(1);
        while Instant::now() < next && !QUIT.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if QUIT.load(Ordering::SeqCst) {
            break;
        }
    }
    if interactive {
        let _ = writeln!(out, "\x1b[?25h");
        let _ = out.flush();
    }
    0
}

fn tab_bar(theme: &Theme, width: usize, label: &str, status: &str) -> String {
    let bg = |c: (u8, u8, u8)| format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2);
    let fg = |c: (u8, u8, u8)| format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2);
    let mut row = String::new();
    let mut used = 0usize;
    row.push_str(&bg(theme.bg));
    if label.is_empty() {
        let tabs = [
            (" 1 herdr-stats ", true),
            (" 2 build ", false),
            (" 3 notes ", false),
        ];
        for (tab, active) in tabs {
            if active {
                row.push_str(&format!(
                    "{}{}\x1b[1m{tab}\x1b[22m{}",
                    bg(theme.active_bg),
                    fg(theme.text),
                    bg(theme.bg)
                ));
            } else {
                row.push_str(&format!("{}{tab}", fg(theme.fg)));
            }
            used += tab.len();
        }
        row.push_str(&format!("{} + ", fg(theme.dim)));
        used += 3;
    } else {
        let text = format!(" {label:<12}");
        row.push_str(&format!("{}{text}", fg(theme.dim)));
        used += text.len();
    }
    // A row wider than the terminal would wrap and throw off the in-place
    // redraw. herdr hides the stats when they do not fit; say so instead.
    let needed = used + render::cells(status) + 1;
    let note;
    let (status, colour) = if needed <= width {
        (status, theme.fg)
    } else {
        note = format!("needs {needed} columns");
        (note.as_str(), theme.dim)
    };
    let status_width = render::cells(status);
    if used + status_width < width {
        row.push_str(&" ".repeat(width - used - status_width - 1));
        row.push_str(&fg(colour));
        row.push_str(status);
        row.push(' ');
    } else if used < width {
        row.push_str(&" ".repeat(width - used));
    }
    row.push_str("\x1b[0m");
    row
}

fn terminal_width() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 20 {
        size.ws_col as usize
    } else {
        120
    }
}

/// Herdr's theme tokens from the user's config, with Catppuccin Mocha (herdr's
/// default theme) filling anything unset.
fn load_theme() -> Theme {
    let mut theme = Theme {
        bg: (0x1e, 0x1e, 0x2e),
        fg: (0x7f, 0x84, 0x9c),
        dim: (0x6c, 0x70, 0x86),
        text: (0xcd, 0xd6, 0xf4),
        active_bg: (0x31, 0x32, 0x44),
    };
    let path = crate::setup::herdr_config_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return theme;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return theme;
    };
    let Some(custom) = doc.get("theme").and_then(|t| t.get("custom")) else {
        return theme;
    };
    let color = |key: &str| custom.get(key).and_then(|v| v.as_str()).and_then(hex);
    if let Some(c) = color("panel_bg") {
        theme.bg = c;
    }
    if let Some(c) = color("overlay1") {
        theme.fg = c;
    }
    if let Some(c) = color("overlay0") {
        theme.dim = c;
    }
    if let Some(c) = color("text") {
        theme.text = c;
    }
    if let Some(c) = color("surface0").or_else(|| color("active_row_bg")) {
        theme.active_bg = c;
    }
    theme
}

fn hex(value: &str) -> Option<(u8, u8, u8)> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Synthetic load: a slow swell with bursts, so every visual has motion.
#[derive(Default)]
struct Demo {
    t: f32,
    seed: u32,
    reading: Reading,
    cpu: History,
    gpu: History,
    net: History,
    tick: u64,
}

impl Demo {
    fn noise(&mut self) -> f32 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.seed >> 8) as f32 / (1u32 << 24) as f32
    }

    fn step(&mut self) {
        self.t += 1.0;
        self.tick += 1;
        let t = self.t;
        let swell = ((t / 9.0).sin() * 0.5 + 0.5) * 0.55;
        let burst = if (t as u32 / 7) % 3 == 0 {
            0.35 * self.noise()
        } else {
            0.0
        };
        let cpu = (0.06 + swell + burst + 0.06 * self.noise()).clamp(0.0, 1.0);
        let cores: Vec<f32> = (0..12)
            .map(|i| {
                (cpu * (0.4 + 1.2 * self.noise()) + if i < 4 { 0.1 } else { 0.0 }).clamp(0.0, 1.0)
            })
            .collect();
        let gpu = ((t / 5.0).sin() * 0.5 + 0.5).powf(3.0);
        let gib = 1024.0 * 1024.0 * 1024.0;
        let rx = 10f64.powf(3.0 + 4.5 * self.noise() as f64);
        let tx = 10f64.powf(3.0 + 3.0 * self.noise() as f64);
        self.reading = Reading {
            cpu: Some(cpu),
            cores,
            mem: Some(Mem {
                used: ((17.0 + 6.0 * swell as f64) * gib) as u64,
                total: (32.0 * gib) as u64,
                ..Default::default()
            }),
            disk: Some(Disk {
                used: 412_000_000_000,
                total: 994_000_000_000,
            }),
            temp: Some(Temps {
                cpu: Some(46.0 + 48.0 * cpu),
                ..Default::default()
            }),
            gpu: Some(gpu),
            net: Some(Net { rx, tx }),
            power_w: Some(6.0 + 38.0 * cpu),
            fan: Some(Fan {
                rpm: if cpu > 0.5 {
                    1200.0 + 3000.0 * cpu
                } else {
                    0.0
                },
            }),
            battery: Some(Battery {
                fraction: 0.85,
                charging: false,
                on_ac: true,
            }),
            load1: Some(1.0 + 9.0 * cpu),
        };
        self.cpu.push(cpu);
        self.gpu.push(gpu);
        self.net.push(render::net_level(rx + tx));
    }

    fn view(&self) -> View<'_> {
        View {
            reading: &self.reading,
            cpu: &self.cpu,
            gpu: &self.gpu,
            net: &self.net,
            tick: self.tick,
        }
    }
}
