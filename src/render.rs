//! Turns a reading into the one line herdr draws at the right of its tab bar.
//!
//! Herdr draws that line in a single theme colour and strips escape sequences,
//! so everything here is shape, not colour: braille graphs and gauges that sit
//! on a shared dotted baseline, icons that change with the reading, and fixed
//! field widths so the tabs beside the line never shift as numbers change.

use crate::config::{Alerts, Amount, Graph, Icons, Layout, Module, Sensor, Style};
use crate::sample::{Pressure, Reading};
use std::collections::VecDeque;
use std::fmt::Write;

/// Blank braille: invisible, one cell wide, and not whitespace, so herdr's
/// trimming leaves it alone and a graph that is still filling keeps its width.
const BLANK: char = '\u{2800}';

/// Braille dots per column, bottom to top: a column of height `h` lights the
/// first `h` dots. Left column uses dots 7,3,2,1; right column 8,6,5,4.
const LEFT: [u32; 5] = [0x00, 0x40, 0x44, 0x46, 0x47];
const RIGHT: [u32; 5] = [0x00, 0x80, 0xA0, 0xB0, 0xB8];
const BLOCKS: [char; 9] = [BLANK, '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// herdr keeps this many characters of the line and drops the rest.
pub const MAX_CHARS: usize = 80;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const GB: f64 = 1_000_000_000.0;

/// Recent samples for a scrolling graph, newest last.
#[derive(Debug, Clone, Default)]
pub struct History {
    samples: VecDeque<f32>,
}

impl History {
    const CAPACITY: usize = 96;

    pub fn push(&mut self, value: f32) {
        if self.samples.len() == Self::CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(value.clamp(0.0, 1.0));
    }

    /// The last `n` samples, left-padded with `None` while history is short.
    fn tail(&self, n: usize) -> impl Iterator<Item = Option<f32>> + '_ {
        let have = self.samples.len().min(n);
        let skip = self.samples.len() - have;
        std::iter::repeat_n(None, n - have).chain(self.samples.iter().skip(skip).map(|v| Some(*v)))
    }
}

pub struct View<'a> {
    pub reading: &'a Reading,
    pub cpu: &'a History,
    pub gpu: &'a History,
    pub net: &'a History,
    /// Advances once per sample; drives blinking alerts.
    pub tick: u64,
}

/// The line for a layout. Modules that would run past herdr's 80 characters
/// are left out whole, from the end, rather than cut mid-number by herdr.
pub fn render(layout: &Layout, view: &View) -> String {
    let mut line = String::with_capacity(256);
    let mut chars = 0;
    for module in &layout.modules {
        let mut part = String::with_capacity(64);
        if !render_module(&mut part, layout, module, view) {
            continue;
        }
        let gap = if chars == 0 { 0 } else { layout.gap };
        let len = part.chars().count();
        if chars + gap + len > MAX_CHARS {
            break;
        }
        line.extend(std::iter::repeat_n(' ', gap));
        line.push_str(&part);
        chars += gap + len;
    }
    line
}

/// Returns false when the module has nothing to show on this machine (no
/// battery, no GPU counters), so it drops out with its gap.
fn render_module(out: &mut String, layout: &Layout, module: &Module, view: &View) -> bool {
    let reading = view.reading;
    match *module {
        Module::Cpu { graph, width } => {
            icon(out, layout, view, Kind::Cpu, false);
            match graph {
                Graph::History => {
                    history(out, view.cpu, width, layout.style);
                    out.push(' ');
                }
                Graph::Cores => {
                    cores(out, &reading.cores, layout.style);
                    out.push(' ');
                }
                Graph::None => {}
            }
            percent(out, reading.cpu);
        }
        Module::Gpu { graph, width } => {
            if reading.gpu.is_none() {
                return false;
            }
            icon(out, layout, view, Kind::Gpu, false);
            if graph != Graph::None {
                history(out, view.gpu, width, layout.style);
                out.push(' ');
            }
            percent(out, reading.gpu);
        }
        Module::Mem {
            gauge: cells,
            value,
        } => {
            let mem = reading.mem;
            let alert = mem.is_some_and(|m| m.pressure == Pressure::Critical);
            icon(out, layout, view, Kind::Mem, alert);
            if cells > 0 {
                gauge(
                    out,
                    mem.map(|m| m.used as f32 / m.total.max(1) as f32),
                    cells,
                    layout.style,
                );
                out.push(' ');
            }
            match mem {
                Some(m) => binary_amount(out, m.used, m.total, value),
                None => out.push_str("--"),
            }
        }
        Module::Disk {
            gauge: cells,
            value,
        } => {
            let disk = reading.disk;
            let fraction = disk.map(|d| d.used as f32 / d.total.max(1) as f32);
            icon(
                out,
                layout,
                view,
                Kind::Disk,
                fraction.is_some_and(|f| f >= 0.95),
            );
            if cells > 0 {
                gauge(out, fraction, cells, layout.style);
                out.push(' ');
            }
            match disk {
                Some(d) => decimal_amount(out, d.used, d.total, value),
                None => out.push_str("--"),
            }
        }
        Module::Temp {
            sensor,
            fahrenheit,
            warm,
            hot,
            critical,
        } => {
            // No sensors at all (most VMs): hide, rather than show "--°" forever.
            if reading.temp.is_none() {
                return false;
            }
            let celsius = reading.temp.and_then(|t| match sensor {
                Sensor::Hottest => t.hottest(),
                Sensor::Cpu => t.cpu,
                Sensor::Gpu => t.gpu,
                Sensor::Soc => t.soc,
            });
            let kind = match celsius {
                Some(c) if c >= hot => Kind::TempHot,
                Some(c) if c < warm => Kind::TempCool,
                _ => Kind::Temp,
            };
            icon(
                out,
                layout,
                view,
                kind,
                celsius.is_some_and(|c| c >= critical),
            );
            match celsius {
                Some(c) => {
                    let shown = if fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c };
                    // Three digits drop the degree sign, so the width holds.
                    if shown.round() >= 100.0 {
                        let _ = write!(out, "{:.0}", shown.min(999.0));
                    } else {
                        let _ = write!(out, "{:>2.0}°", shown.max(0.0));
                    }
                    if fahrenheit {
                        out.push('F');
                    }
                }
                None => out.push_str("--°"),
            }
        }
        Module::Net { graph } => {
            icon(out, layout, view, Kind::Net, false);
            if graph > 0 {
                history(out, view.net, graph, layout.style);
                out.push(' ');
            }
            let net = reading.net;
            out.push('↓');
            rate(out, net.map(|n| n.rx));
            out.push(' ');
            out.push('↑');
            rate(out, net.map(|n| n.tx));
        }
        Module::Power => {
            let Some(watts) = reading.power_w else {
                return false;
            };
            icon(out, layout, view, Kind::Power, false);
            // Three digits drop the unit, so the width holds.
            if watts.round() >= 100.0 {
                let _ = write!(out, "{:.0}", watts.min(999.0));
            } else {
                let _ = write!(out, "{:>2.0}W", watts.max(0.0));
            }
        }
        Module::Fan => {
            let Some(fan) = reading.fan else { return false };
            let spinning = fan.rpm >= 1.0;
            icon(
                out,
                layout,
                view,
                if spinning { Kind::Fan } else { Kind::FanOff },
                false,
            );
            if spinning && fan.rpm < 9_950.0 {
                let _ = write!(out, "{:.1}k", fan.rpm / 1000.0);
            } else if spinning {
                let _ = write!(out, "{:>3.0}k", (fan.rpm / 1000.0).min(999.0));
            } else {
                out.push_str(" off");
            }
        }
        Module::Battery => {
            let Some(battery) = reading.battery else {
                return false;
            };
            let kind = if battery.charging || battery.on_ac {
                Kind::BatteryCharging
            } else {
                Kind::Battery(battery.fraction)
            };
            icon(
                out,
                layout,
                view,
                kind,
                !battery.on_ac && battery.fraction < 0.1,
            );
            // Three digits drop the unit, so a full battery keeps the width.
            let percent = (battery.fraction * 100.0).round().clamp(0.0, 100.0);
            if percent >= 100.0 {
                out.push_str("100");
            } else {
                let _ = write!(out, "{percent:>2.0}%");
            }
        }
        Module::Load => {
            let Some(load) = reading.load1 else {
                return false;
            };
            icon(out, layout, view, Kind::Load, false);
            if load < 9.995 {
                let _ = write!(out, "{:.2}", load.max(0.0));
            } else if load < 99.95 {
                let _ = write!(out, "{load:>4.1}");
            } else {
                let _ = write!(out, "{:>4.0}", load.min(9999.0));
            }
        }
    }
    true
}

#[derive(Clone, Copy)]
enum Kind {
    Cpu,
    Gpu,
    Mem,
    Disk,
    Temp,
    TempCool,
    TempHot,
    Net,
    Power,
    Fan,
    FanOff,
    Battery(f32),
    BatteryCharging,
    Load,
}

impl Kind {
    fn nerd(self) -> char {
        match self {
            Self::Cpu => '\u{f0ee0}',      // md-cpu_64_bit
            Self::Gpu => '\u{f08ae}',      // md-expansion_card
            Self::Mem => '\u{f035b}',      // md-memory
            Self::Disk => '\u{f02ca}',     // md-harddisk
            Self::Temp => '\u{f050f}',     // md-thermometer
            Self::TempCool => '\u{f10c3}', // md-thermometer_low
            Self::TempHot => '\u{f10c2}',  // md-thermometer_high
            Self::Net => '\u{f06f3}',      // md-network
            Self::Power => '\u{f140b}',    // md-lightning_bolt
            Self::Fan => '\u{f0210}',      // md-fan
            Self::FanOff => '\u{f081d}',   // md-fan_off
            Self::Load => '\u{f04c5}',     // md-speedometer
            Self::BatteryCharging => '\u{f0084}',
            // md-battery_10 … md-battery_90 are consecutive; md-battery is full.
            Self::Battery(fraction) => {
                let tenth = (fraction * 10.0).round().clamp(1.0, 10.0) as u32;
                if tenth >= 10 {
                    '\u{f0079}'
                } else {
                    char::from_u32(0xf007a + tenth - 1).unwrap_or('\u{f0079}')
                }
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Mem => "mem",
            Self::Disk => "ssd",
            Self::Temp | Self::TempCool | Self::TempHot => "tmp",
            Self::Net => "net",
            Self::Power => "pwr",
            Self::Fan | Self::FanOff => "fan",
            Self::Battery(_) | Self::BatteryCharging => "bat",
            Self::Load => "load",
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Self::Temp | Self::TempCool | Self::TempHot => "🔥",
            Self::Battery(_) | Self::BatteryCharging => "🪫",
            _ => "🔴",
        }
    }
}

const ALERT: char = '\u{f0026}'; // md-alert

fn icon(out: &mut String, layout: &Layout, view: &View, kind: Kind, alert: bool) {
    let alerting = alert
        && match layout.alerts {
            Alerts::Off => false,
            Alerts::Blink => view.tick % 2 == 0,
            Alerts::Icon | Alerts::Emoji => true,
        };
    match layout.icons {
        Icons::None => {}
        Icons::Nerd => {
            if alerting && layout.alerts == Alerts::Emoji {
                // An emoji is two cells: exactly the icon plus its space.
                out.push_str(kind.emoji());
            } else {
                out.push(if alerting { ALERT } else { kind.nerd() });
                out.push(' ');
            }
        }
        Icons::Text => {
            if alerting {
                out.push_str(&kind.label().to_ascii_uppercase());
            } else {
                out.push_str(kind.label());
            }
            out.push(' ');
        }
    }
}

fn level4(value: f32) -> usize {
    // 1 is the resting baseline, so an idle graph still reads as a graph.
    1 + (value.clamp(0.0, 1.0) * 3.0).round() as usize
}

fn level8(value: f32) -> usize {
    1 + (value.clamp(0.0, 1.0) * 7.0).round() as usize
}

fn braille(bits: u32) -> char {
    char::from_u32(0x2800 + bits).unwrap_or(BLANK)
}

/// Scrolling history, newest on the right. Braille packs two samples per cell.
fn history(out: &mut String, history: &History, cells: usize, style: Style) {
    match style {
        Style::Braille => {
            let samples: Vec<Option<f32>> = history.tail(cells * 2).collect();
            for pair in samples.chunks(2) {
                let left = pair[0].map_or(0, level4);
                let right = pair.get(1).copied().flatten().map_or(0, level4);
                out.push(braille(LEFT[left] | RIGHT[right]));
            }
        }
        Style::Blocks => {
            for sample in history.tail(cells) {
                out.push(sample.map_or(BLANK, |v| BLOCKS[level8(v)]));
            }
        }
    }
}

/// One column per core, like a tiny equalizer.
fn cores(out: &mut String, cores: &[f32], style: Style) {
    match style {
        Style::Braille => {
            for pair in cores.chunks(2) {
                let left = level4(pair[0]);
                let right = pair.get(1).map_or(0, |v| level4(*v));
                out.push(braille(LEFT[left] | RIGHT[right]));
            }
        }
        Style::Blocks => {
            for core in cores {
                out.push(BLOCKS[level8(*core)]);
            }
        }
    }
}

/// A level gauge on the same dotted baseline the graphs use: `⣿⣿⣇⣀`.
fn gauge(out: &mut String, fraction: Option<f32>, cells: usize, style: Style) {
    let fraction = fraction.unwrap_or(0.0).clamp(0.0, 1.0);
    match style {
        Style::Braille => {
            let filled = (fraction * (cells * 2) as f32).round() as usize;
            for cell in 0..cells {
                out.push(match filled.saturating_sub(cell * 2).min(2) {
                    2 => braille(0xFF),
                    1 => braille(LEFT[4] | RIGHT[1]),
                    _ => braille(LEFT[1] | RIGHT[1]),
                });
            }
        }
        Style::Blocks => {
            let filled = (fraction * cells as f32).round() as usize;
            for cell in 0..cells {
                out.push(if cell < filled { '▰' } else { '▱' });
            }
        }
    }
}

/// Always three cells. A saturated reading shows 99%: the graph beside it
/// already says "pegged", and a fourth digit would shift the tabs.
fn percent(out: &mut String, fraction: Option<f32>) {
    match fraction {
        Some(f) => {
            let pct = (f.clamp(0.0, 1.0) * 100.0).round().min(99.0);
            let _ = write!(out, "{:>2.0}%", pct);
        }
        None => out.push_str("--%"),
    }
}

/// Memory, in GiB the way Activity Monitor counts it.
fn binary_amount(out: &mut String, used: u64, total: u64, value: Amount) {
    let used_g = used as f64 / GIB;
    let total_g = total as f64 / GIB;
    match value {
        Amount::Percent => percent(out, Some((used_g / total_g.max(f64::EPSILON)) as f32)),
        Amount::UsedTotal => {
            let _ = write!(out, "{:>4}/{}G", one_decimal(used_g), whole_or_one(total_g));
        }
        Amount::Used => {
            let _ = write!(out, "{:>4}G", one_decimal(used_g));
        }
        Amount::Free => {
            let _ = write!(out, "{:>4}G free", one_decimal((total_g - used_g).max(0.0)));
        }
    }
}

/// Disks, in decimal GB/TB the way Finder and the label on the box count.
fn decimal_amount(out: &mut String, used: u64, total: u64, value: Amount) {
    let fraction = used as f64 / total.max(1) as f64;
    let terabytes = total as f64 >= 1000.0 * GB;
    let (scale, unit) = if terabytes {
        (1000.0 * GB, 'T')
    } else {
        (GB, 'G')
    };
    let used_u = used as f64 / scale;
    let total_u = total as f64 / scale;
    let free_u = (total_u - used_u).max(0.0);
    // Width of the total, so used and free never change the field's width.
    let digits = if terabytes {
        one_decimal(total_u).len()
    } else {
        format!("{total_u:.0}").len()
    };
    let fmt = |v: f64| {
        if terabytes {
            one_decimal(v)
        } else {
            format!("{v:.0}")
        }
    };
    match value {
        Amount::Percent => percent(out, Some(fraction as f32)),
        Amount::UsedTotal => {
            let _ = write!(out, "{:>digits$}/{}{unit}", fmt(used_u), fmt(total_u));
        }
        Amount::Used => {
            let _ = write!(out, "{:>digits$}{unit}", fmt(used_u));
        }
        Amount::Free => {
            let _ = write!(out, "{:>digits$}{unit} free", fmt(free_u));
        }
    }
}

/// `9.8`, `18.4`, `118`: three significant figures at most, four cells.
fn one_decimal(value: f64) -> String {
    if value < 99.95 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

fn whole_or_one(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// Bytes per second in four cells: `512B`, `1.2K`, ` 34K`, `340M`.
fn rate(out: &mut String, bytes: Option<f64>) {
    let Some(mut value) = bytes else {
        out.push_str("   -");
        return;
    };
    let units = ['B', 'K', 'M', 'G', 'T'];
    let mut unit = 0;
    while value >= 999.5 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit > 0 && value < 9.95 {
        let _ = write!(out, "{value:.1}{}", units[unit]);
    } else {
        let _ = write!(out, "{:>3.0}{}", value, units[unit]);
    }
}

/// Network history is log-scaled: 1 KB/s at the baseline, 100 MB/s at the top,
/// so both a chat app and a big download read as something.
pub fn net_level(bytes_per_second: f64) -> f32 {
    let floor = 1_000f64.log10();
    let ceiling = 100_000_000f64.log10();
    ((bytes_per_second.max(1.0).log10() - floor) / (ceiling - floor)).clamp(0.0, 1.0) as f32
}

/// Display width in terminal cells. Everything this module emits is one cell
/// per char except the alert emoji.
pub fn cells(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if (c as u32) >= 0x1F000 && (c as u32) < 0x1FB00 {
                2
            } else {
                1
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, File};
    use crate::sample::{Disk, Mem, Temps};

    fn layout(preset: &str) -> Layout {
        config::resolve(&File::default(), Some(preset)).unwrap()
    }

    fn reading(cpu: f32, used_gib: f64, temp: f32) -> Reading {
        Reading {
            cpu: Some(cpu),
            cores: vec![cpu; 12],
            mem: Some(Mem {
                used: (used_gib * GIB) as u64,
                total: 32 * 1024 * 1024 * 1024,
                ..Default::default()
            }),
            disk: Some(Disk {
                used: 412_000_000_000,
                total: 994_000_000_000,
            }),
            temp: Some(Temps {
                cpu: Some(temp),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn line(preset: &str, r: &Reading, h: &History) -> String {
        let empty = History::default();
        render(
            &layout(preset),
            &View {
                reading: r,
                cpu: h,
                gpu: &empty,
                net: &empty,
                tick: 0,
            },
        )
    }

    #[test]
    fn width_does_not_depend_on_the_numbers() {
        let mut history = History::default();
        for preset in config::PRESETS {
            let idle = line(preset, &reading(0.03, 5.2, 41.0), &history);
            for i in 0..40 {
                history.push((i as f32 / 40.0).sin().abs());
            }
            let busy = line(preset, &reading(1.0, 31.9, 99.0), &history);
            assert_eq!(cells(&idle), cells(&busy), "{preset}: `{idle}` vs `{busy}`");
        }
    }

    #[test]
    fn every_preset_fits_herdrs_80_character_limit() {
        let mut history = History::default();
        for _ in 0..40 {
            history.push(0.5);
        }
        let mut r = reading(1.0, 31.9, 99.0);
        r.gpu = Some(1.0);
        r.power_w = Some(88.0);
        r.net = Some(crate::sample::Net {
            rx: 123e6,
            tx: 45e6,
        });
        for preset in config::PRESETS {
            let text = line(preset, &r, &history);
            assert!(
                text.chars().count() <= 80,
                "{preset} is {} chars: `{text}`",
                text.chars().count()
            );
        }
    }

    #[test]
    fn idle_graph_rests_on_the_baseline() {
        let mut history = History::default();
        for _ in 0..16 {
            history.push(0.0);
        }
        let mut out = String::new();
        super::history(&mut out, &history, 4, Style::Braille);
        assert_eq!(out, "⣀⣀⣀⣀");
    }

    #[test]
    fn short_history_fills_in_from_the_right() {
        let mut history = History::default();
        history.push(1.0);
        let mut out = String::new();
        super::history(&mut out, &history, 3, Style::Braille);
        assert_eq!(out, "\u{2800}\u{2800}⢸");
    }

    #[test]
    fn gauge_steps_in_half_cells() {
        let mut out = String::new();
        gauge(&mut out, Some(0.5), 3, Style::Braille);
        assert_eq!(out, "⣿⣇⣀");
    }

    #[test]
    fn memory_and_disk_formats() {
        let mut out = String::new();
        binary_amount(
            &mut out,
            (18.43 * GIB) as u64,
            32 * 1024 * 1024 * 1024,
            Amount::UsedTotal,
        );
        assert_eq!(out, "18.4/32G");
        out.clear();
        binary_amount(
            &mut out,
            (9.8 * GIB) as u64,
            32 * 1024 * 1024 * 1024,
            Amount::UsedTotal,
        );
        assert_eq!(out, " 9.8/32G");
        out.clear();
        decimal_amount(
            &mut out,
            412_000_000_000,
            994_000_000_000,
            Amount::UsedTotal,
        );
        assert_eq!(out, "412/994G");
        out.clear();
        decimal_amount(&mut out, 31_000_000_000, 994_000_000_000, Amount::UsedTotal);
        assert_eq!(out, " 31/994G");
        out.clear();
        decimal_amount(
            &mut out,
            1_200_000_000_000,
            2_000_000_000_000,
            Amount::UsedTotal,
        );
        assert_eq!(out, "1.2/2.0T");
    }

    #[test]
    fn rates_stay_four_cells() {
        for bytes in [
            0.0,
            512.0,
            999.0,
            1_000.0,
            34_000.0,
            1_200_000.0,
            340_000_000.0,
            5e12,
        ] {
            let mut out = String::new();
            rate(&mut out, Some(bytes));
            assert_eq!(cells(&out), 4, "{bytes} -> `{out}`");
        }
    }

    /// One module rendered from a config snippet, the way a user would set it.
    fn module(config: &str, reading: &Reading) -> String {
        let file: File = toml::from_str(config).unwrap();
        let layout = config::resolve(&file, None).unwrap();
        let empty = History::default();
        render(
            &layout,
            &View {
                reading,
                cpu: &empty,
                gpu: &empty,
                net: &empty,
                tick: 0,
            },
        )
    }

    fn assert_one_width(name: &str, lines: &[String]) {
        let widths: Vec<usize> = lines.iter().map(|l| cells(l)).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "{name} widths differ: {lines:?}"
        );
    }

    #[test]
    fn extreme_readings_keep_every_module_width() {
        use crate::sample::{Battery, Fan};
        let temps = |unit: &str| -> Vec<String> {
            [1.0, 9.4, 41.0, 37.4, 37.6, 99.4, 99.6, 105.0, 150.0]
                .iter()
                .map(|c| {
                    let r = Reading {
                        temp: Some(Temps {
                            cpu: Some(*c),
                            ..Default::default()
                        }),
                        ..Default::default()
                    };
                    module(
                        &format!("modules = [\"temp\"]\n[temp]\nunit = \"{unit}\""),
                        &r,
                    )
                })
                .collect()
        };
        assert_one_width("celsius", &temps("c"));
        assert_one_width("fahrenheit", &temps("f"));

        let power: Vec<String> = [0.4, 3.0, 19.0, 99.4, 99.6, 150.0, 2000.0]
            .iter()
            .map(|w| {
                module(
                    "modules = [\"power\"]",
                    &Reading {
                        power_w: Some(*w),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert_one_width("power", &power);

        let battery: Vec<String> = [0.04, 0.5, 0.85, 0.996, 1.0]
            .iter()
            .map(|f| {
                let b = Battery {
                    fraction: *f,
                    charging: false,
                    on_ac: false,
                };
                module(
                    "modules = [\"battery\"]",
                    &Reading {
                        battery: Some(b),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert_one_width("battery", &battery);

        let load: Vec<String> = [0.0, 1.5, 9.996, 12.3, 99.96, 250.0, 12_000.0]
            .iter()
            .map(|l| {
                module(
                    "modules = [\"load\"]",
                    &Reading {
                        load1: Some(*l),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert_one_width("load", &load);

        let fan: Vec<String> = [0.0, 1200.0, 9_949.0, 9_951.0, 12_000.0]
            .iter()
            .map(|rpm| {
                module(
                    "modules = [\"fan\"]",
                    &Reading {
                        fan: Some(Fan { rpm: *rpm }),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert_one_width("fan", &fan);
    }

    #[test]
    fn every_preset_and_icon_set_fits_and_full_shows_everything_with_icons() {
        let mut history = History::default();
        for _ in 0..40 {
            history.push(0.5);
        }
        let mut r = reading(1.0, 31.9, 105.0);
        r.gpu = Some(1.0);
        r.power_w = Some(150.0);
        r.net = Some(crate::sample::Net {
            rx: 123e6,
            tx: 45e6,
        });
        for icons in ["nerd", "text", "none"] {
            for preset in config::PRESETS {
                let file: File = toml::from_str(&format!("icons = \"{icons}\"")).unwrap();
                let layout = config::resolve(&file, Some(preset)).unwrap();
                let text = render(
                    &layout,
                    &View {
                        reading: &r,
                        cpu: &history,
                        gpu: &history,
                        net: &history,
                        tick: 0,
                    },
                );
                let count = text.chars().count();
                assert!(
                    count <= MAX_CHARS,
                    "{preset}/{icons} is {count} chars: `{text}`"
                );
                if icons == "nerd" && preset == "full" {
                    assert!(text.contains('↓'), "full lost a module: `{text}`");
                }
            }
        }
    }
}
