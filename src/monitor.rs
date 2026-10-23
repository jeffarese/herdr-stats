//! Sampling state: the probe, recent history, and per-subsystem cadence.
//!
//! Sampling is driven by demand. Nothing runs between requests; each request
//! refreshes whatever has gone stale, so the cost tracks how often herdr asks
//! and drops to zero when it stops asking.

use crate::config::{self, Layout, Needs};
use crate::render::{self, History, View};
use crate::sample::{Probe, Reading};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Closer than this and two requests share a sample, so two herdr sessions
/// polling the same daemon do not double the graph's speed.
const FAST_EVERY: Duration = Duration::from_millis(900);
const THERMAL_EVERY: Duration = Duration::from_secs(1);
const POWER_EVERY: Duration = Duration::from_secs(2);
const BATTERY_EVERY: Duration = Duration::from_secs(15);
/// Finder-style capacity asks the system about purgeable space: ~7 ms.
const DISK_EVERY: Duration = Duration::from_secs(60);
const CONFIG_EVERY: Duration = Duration::from_secs(2);
/// Smoothing for the temperature: sensors jump several degrees between reads.
const TEMP_SMOOTHING: f32 = 0.45;
/// How long a module keeps its last value through failed reads before it
/// drops out of the line (which shifts everything after it).
const HOLD: Duration = Duration::from_secs(10);
/// The first CPU percent is the usage since the probe was created; shorter
/// than this and it is mostly noise.
const FIRST_WINDOW: Duration = Duration::from_millis(150);

pub struct Monitor {
    probe: Probe,
    pub reading: Reading,
    pub cpu: History,
    pub gpu: History,
    pub net: History,
    pub tick: u64,
    sampled_at: Option<Instant>,
    thermal_at: Option<Instant>,
    power_at: Option<Instant>,
    battery_at: Option<Instant>,
    disk_at: Option<Instant>,
    disk_path: String,
    config_path: PathBuf,
    config_at: Option<Instant>,
    config_stamp: Option<SystemTime>,
    file: config::File,
    pub config_error: Option<String>,
    layouts: HashMap<String, Layout>,
    frames: HashMap<String, (u64, String)>,
    /// Smoothed cost of one sampling pass, for `status` and `doctor`.
    pub sample_cost: Duration,
    /// The last preset name a client asked for that does not exist.
    pub unknown_preset: Option<String>,
    created: Instant,
    held: Held,
}

/// When each holdable reading was last read successfully.
#[derive(Default)]
struct Held {
    gpu: Option<Instant>,
    temp: Option<Instant>,
    power: Option<Instant>,
    fan: Option<Instant>,
    battery: Option<Instant>,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            probe: Probe::new(),
            reading: Reading::default(),
            cpu: History::default(),
            gpu: History::default(),
            net: History::default(),
            tick: 0,
            sampled_at: None,
            thermal_at: None,
            power_at: None,
            battery_at: None,
            disk_at: None,
            disk_path: "/".into(),
            config_path: config::path(),
            config_at: None,
            config_stamp: None,
            file: config::File::default(),
            config_error: None,
            layouts: HashMap::new(),
            frames: HashMap::new(),
            sample_cost: Duration::ZERO,
            unknown_preset: None,
            created: Instant::now(),
            held: Held::default(),
        }
    }

    pub fn probe(&mut self) -> &mut Probe {
        &mut self.probe
    }

    /// The rendered line for a preset (empty string: the config's preset).
    pub fn frame(&mut self, preset: &str) -> String {
        // Unknown names get the config's preset, so a typo cannot add cache
        // entries without bound or pass for a config error.
        let preset = if preset.is_empty() || config::PRESETS.contains(&preset) {
            preset
        } else {
            self.unknown_preset = Some(preset.chars().take(32).collect());
            ""
        };
        let now = Instant::now();
        self.refresh_config(now);
        let layout = self.layout(preset);
        let mut needs = self.layouts.values().fold(Needs::default(), union);
        needs = union(needs, &layout);
        self.disk_path.clone_from(&layout.disk_path);
        self.sample(now, needs);

        if let Some((tick, frame)) = self.frames.get(preset) {
            if *tick == self.tick {
                return frame.clone();
            }
        }
        let view = View {
            reading: &self.reading,
            cpu: &self.cpu,
            gpu: &self.gpu,
            net: &self.net,
            tick: self.tick,
        };
        let frame = render::render(&layout, &view);
        self.frames
            .insert(preset.to_string(), (self.tick, frame.clone()));
        frame
    }

    pub fn layout(&mut self, preset: &str) -> Layout {
        if let Some(layout) = self.layouts.get(preset) {
            return layout.clone();
        }
        let layout = match config::resolve(&self.file, Some(preset)) {
            Ok(layout) => layout,
            Err(err) => {
                // A typo in the file must not blank the tab bar: show the
                // preset as shipped and surface the error in `status`.
                self.config_error = Some(err);
                config::resolve(&config::File::default(), Some(preset))
                    .or_else(|_| config::resolve(&config::File::default(), None))
                    .expect("built-in presets resolve")
            }
        };
        self.layouts.insert(preset.to_string(), layout.clone());
        layout
    }

    fn refresh_config(&mut self, now: Instant) {
        if self
            .config_at
            .is_some_and(|at| now.duration_since(at) < CONFIG_EVERY)
        {
            return;
        }
        let first = self.config_at.is_none();
        self.config_at = Some(now);
        let stamp = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        if !first && stamp == self.config_stamp {
            return;
        }
        self.config_stamp = stamp;
        match config::load(&self.config_path) {
            Ok(file) => {
                self.file = file;
                self.config_error = None;
            }
            Err(err) => self.config_error = Some(err),
        }
        if !first {
            self.layouts.clear();
            self.frames.clear();
        }
    }

    fn sample(&mut self, now: Instant, needs: Needs) {
        let due = |at: Option<Instant>, every: Duration| {
            at.is_none_or(|at| now.duration_since(at) >= every)
        };
        if !due(self.sampled_at, FAST_EVERY) {
            return;
        }
        let first = self.sampled_at.is_none();
        if first {
            std::thread::sleep(FIRST_WINDOW.saturating_sub(self.created.elapsed()));
        }
        let started = Instant::now();
        self.sampled_at = Some(now);
        self.tick += 1;
        let probe = &mut self.probe;
        let reading = &mut self.reading;
        let held = &mut self.held;

        if needs.cpu {
            probe.cpu(reading);
            if let Some(cpu) = reading.cpu {
                self.cpu.push(cpu);
            }
        }
        if needs.mem {
            probe.memory(reading);
        }
        if needs.gpu {
            let previous = reading.gpu;
            probe.gpu(reading);
            if let Some(gpu) = reading.gpu {
                self.gpu.push(gpu);
            }
            hold(&mut reading.gpu, previous, &mut held.gpu, now);
        }
        if needs.net {
            probe.network(reading);
            if let Some(net) = reading.net {
                self.net.push(render::net_level(net.rx + net.tx));
            }
        }
        if needs.load {
            probe.load(reading);
        }
        if needs.temp && due(self.thermal_at, THERMAL_EVERY) {
            self.thermal_at = Some(now);
            let previous = reading.temp;
            probe.thermal(reading, needs.temp_want);
            if let (Some(prev), Some(next)) = (previous, reading.temp.as_mut()) {
                let blend = |old: Option<f32>, new: &mut Option<f32>| {
                    if let (Some(old), Some(new)) = (old, new.as_mut()) {
                        *new = old + (*new - old) * TEMP_SMOOTHING;
                    }
                };
                blend(prev.cpu, &mut next.cpu);
                blend(prev.gpu, &mut next.gpu);
                blend(prev.soc, &mut next.soc);
            }
            hold(&mut reading.temp, previous, &mut held.temp, now);
        }
        if (needs.power || needs.fan) && due(self.power_at, POWER_EVERY) {
            self.power_at = Some(now);
            let (power, fan) = (reading.power_w, reading.fan);
            probe.power(reading, needs.power, needs.fan);
            hold(&mut reading.power_w, power, &mut held.power, now);
            hold(&mut reading.fan, fan, &mut held.fan, now);
        }
        if needs.battery && due(self.battery_at, BATTERY_EVERY) {
            self.battery_at = Some(now);
            let previous = reading.battery;
            probe.battery(reading);
            hold(&mut reading.battery, previous, &mut held.battery, now);
        }
        if needs.disk && due(self.disk_at, DISK_EVERY) {
            self.disk_at = Some(now);
            probe.disk(reading, &self.disk_path);
        }

        let cost = started.elapsed();
        self.sample_cost = if first {
            cost
        } else {
            (self.sample_cost * 7 + cost) / 8
        };
    }
}

/// Keeps `previous` in `slot` when this read failed and the last good one is
/// recent; records the time of a good read.
fn hold<T: Copy>(
    slot: &mut Option<T>,
    previous: Option<T>,
    ok_at: &mut Option<Instant>,
    now: Instant,
) {
    if slot.is_some() {
        *ok_at = Some(now);
    } else if ok_at.is_some_and(|at| now.duration_since(at) < HOLD) {
        *slot = previous;
    }
}

fn union(acc: Needs, layout: &Layout) -> Needs {
    let n = layout.needs();
    Needs {
        temp_want: crate::sample::TempWant {
            cpu: acc.temp_want.cpu || n.temp_want.cpu,
            gpu: acc.temp_want.gpu || n.temp_want.gpu,
            soc: acc.temp_want.soc || n.temp_want.soc,
        },
        cpu: acc.cpu || n.cpu,
        gpu: acc.gpu || n.gpu,
        mem: acc.mem || n.mem,
        disk: acc.disk || n.disk,
        temp: acc.temp || n.temp,
        net: acc.net || n.net,
        power: acc.power || n.power,
        fan: acc.fan || n.fan,
        battery: acc.battery || n.battery,
        load: acc.load || n.load,
    }
}
