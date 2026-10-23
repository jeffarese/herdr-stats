//! Linux samplers: procfs and sysfs, read through file handles kept open and
//! re-read from offset zero, so a sample costs a few `pread`s and no opens.
//! The text is parsed in `procfs.rs`, which is tested on every platform.

use super::procfs::{self, CpuCounter, NetCounters, Supply};
use super::{Reading, TempWant, Temps};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the interface list is rebuilt, so docks and USB tethering that
/// appear after start are counted.
const IFACES_EVERY: Duration = Duration::from_secs(30);
/// How often /sys/class/power_supply is rescanned for batteries.
const BATTERIES_EVERY: Duration = Duration::from_secs(15);
/// Reads stop growing the buffer here; procfs text is far smaller.
const READ_LIMIT: usize = 4 << 20;

pub struct Probe {
    buffer: Vec<u8>,
    stat: Option<File>,
    meminfo: Option<File>,
    netdev: Option<File>,
    cpu: CpuCounter,
    net: NetCounters,
    net_at: Option<Instant>,
    ifaces_at: Option<Instant>,
    cpu_temp: Option<(PathBuf, File)>,
    gpu: Option<Gpu>,
    batteries: Vec<(String, File)>,
    batteries_at: Option<Instant>,
    supplies: Vec<Supply>,
}

/// An amdgpu device: its busy percentage and its runtime-PM state.
struct Gpu {
    dir: PathBuf,
    busy: File,
    runtime_status: Option<File>,
}

impl Probe {
    pub fn new() -> Self {
        let mut probe = Self {
            buffer: vec![0; 64 * 1024],
            stat: File::open("/proc/stat").ok(),
            meminfo: File::open("/proc/meminfo").ok(),
            netdev: File::open("/proc/net/dev").ok(),
            cpu: CpuCounter::default(),
            net: NetCounters::default(),
            net_at: None,
            ifaces_at: None,
            cpu_temp: find_cpu_temp()
                .and_then(|path| File::open(&path).ok().map(|file| (path, file))),
            gpu: find_gpu(),
            batteries: Vec::new(),
            batteries_at: None,
            supplies: Vec::new(),
        };
        // Prime the tick counters so the first real sample has a delta.
        let mut scratch = Reading::default();
        probe.cpu(&mut scratch);
        probe
    }

    pub fn cpu(&mut self, out: &mut Reading) {
        out.cpu = match self
            .stat
            .as_ref()
            .and_then(|file| read_all(&mut self.buffer, file))
        {
            Some(text) => self.cpu.update(text, &mut out.cores),
            None => {
                out.cores.clear();
                None
            }
        };
    }

    pub fn memory(&mut self, out: &mut Reading) {
        out.mem = self
            .meminfo
            .as_ref()
            .and_then(|file| read_all(&mut self.buffer, file))
            .and_then(procfs::parse_meminfo);
    }

    pub fn gpu(&mut self, out: &mut Reading) {
        let Some(gpu) = &self.gpu else {
            out.gpu = None;
            return;
        };
        // Reading gpu_busy_percent wakes a runtime-suspended GPU on older
        // kernels, which would keep a laptop's dGPU powered every second.
        // A suspended GPU is idle, so report that without touching it.
        let suspended = gpu
            .runtime_status
            .as_ref()
            .and_then(|file| read_attr(&mut self.buffer, file))
            .is_some_and(|status| status.trim() == "suspended");
        out.gpu = if suspended {
            Some(0.0)
        } else {
            read_attr(&mut self.buffer, &gpu.busy).and_then(procfs::busy_percent)
        };
    }

    pub fn network(&mut self, out: &mut Reading) {
        let now = Instant::now();
        if self
            .ifaces_at
            .is_none_or(|at| now.duration_since(at) >= IFACES_EVERY)
        {
            self.net.set_interfaces(physical_interfaces());
            self.ifaces_at = Some(now);
        }
        let Some(text) = self
            .netdev
            .as_ref()
            .and_then(|file| read_all(&mut self.buffer, file))
        else {
            out.net = None;
            return;
        };
        let (rx, tx) = self.net.update(text);
        if let Some(then) = self.net_at {
            let seconds = now.duration_since(then).as_secs_f64();
            if seconds > 0.05 {
                out.net = Some(super::Net {
                    rx: rx as f64 / seconds,
                    tx: tx as f64 / seconds,
                });
            }
        }
        self.net_at = Some(now);
    }

    pub fn load(&mut self, out: &mut Reading) {
        let mut load = [0f64; 3];
        if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } > 0 {
            out.load1 = Some(load[0] as f32);
        }
    }

    pub fn thermal(&mut self, out: &mut Reading, _want: TempWant) {
        out.temp = self
            .cpu_temp
            .as_ref()
            .and_then(|(_, file)| read_attr(&mut self.buffer, file))
            .and_then(procfs::millidegrees)
            .map(|celsius| Temps {
                cpu: Some(celsius),
                ..Default::default()
            });
    }

    /// Battery discharge power only: Linux exposes no whole-system draw, and
    /// a battery's `power_now` on AC is its charge rate or nothing, so the
    /// module shows while discharging and disappears on AC.
    pub fn power(&mut self, out: &mut Reading, want_power: bool, _want_fan: bool) {
        if want_power {
            self.read_supplies();
            out.power_w = procfs::discharge_watts(&self.supplies);
        }
    }

    pub fn battery(&mut self, out: &mut Reading) {
        self.read_supplies();
        out.battery = procfs::combine_batteries(&self.supplies);
    }

    pub fn disk(&mut self, out: &mut Reading, path: &str) {
        out.disk = super::statfs_disk(path);
    }

    pub fn describe(&mut self) -> Vec<String> {
        let path = |p: Option<&Path>| p.map_or("none".into(), |p| p.display().to_string());
        self.read_supplies();
        self.net.set_interfaces(physical_interfaces());
        self.ifaces_at = Some(Instant::now());
        let interfaces: Vec<&str> = self.net.names().collect();
        vec![
            format!("cpu      {} logical cores", self.cpu.cores()),
            format!(
                "thermal  {}",
                path(self.cpu_temp.as_ref().map(|(p, _)| p.as_path()))
            ),
            format!(
                "gpu      {}",
                path(self.gpu.as_ref().map(|gpu| gpu.dir.as_path()))
            ),
            format!("network  {}", interfaces.join(" ")),
            format!(
                "battery  {}",
                if self.batteries.is_empty() {
                    "none".to_string()
                } else {
                    self.batteries
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            ),
        ]
    }

    /// Re-reads every system battery's `uevent` into `supplies`, rescanning
    /// /sys/class/power_supply when the list is stale.
    fn read_supplies(&mut self) {
        let now = Instant::now();
        if self
            .batteries_at
            .is_none_or(|at| now.duration_since(at) >= BATTERIES_EVERY)
        {
            self.batteries = find_batteries(&mut self.buffer);
            self.batteries_at = Some(now);
        }
        self.supplies.clear();
        for (_, file) in &self.batteries {
            if let Some(text) = read_attr(&mut self.buffer, file) {
                self.supplies.push(procfs::parse_uevent(text));
            }
        }
    }
}

/// The whole file from offset zero. procfs files are seq_files: one read can
/// stop at a page (/proc/net/dev with many interfaces, /proc/stat on large
/// machines), so this reads until a read returns nothing, growing the buffer
/// as needed.
fn read_all<'a>(buffer: &'a mut Vec<u8>, file: &File) -> Option<&'a str> {
    let mut len = 0;
    loop {
        if len == buffer.len() {
            if len >= READ_LIMIT {
                break;
            }
            buffer.resize((len * 2).max(4096), 0);
        }
        match file.read_at(&mut buffer[len..], len as u64) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    std::str::from_utf8(&buffer[..len]).ok()
}

/// A sysfs attribute: sysfs hands over the whole value (at most a page) in
/// one read, so this skips the extra read `read_all` makes to find the end.
fn read_attr<'a>(buffer: &'a mut [u8], file: &File) -> Option<&'a str> {
    let len = file.read_at(buffer, 0).ok()?;
    std::str::from_utf8(&buffer[..len]).ok()
}

fn read_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn sorted_entries(dir: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

/// The CPU package sensor: the best-ranked hwmon chip's package input, else
/// the best-ranked thermal zone.
fn find_cpu_temp() -> Option<PathBuf> {
    let mut best: Option<(u8, PathBuf)> = None;
    for dir in sorted_entries("/sys/class/hwmon") {
        let Some(rank) = procfs::hwmon_rank(read_string(&dir.join("name")).trim()) else {
            continue;
        };
        if best
            .as_ref()
            .is_some_and(|(best_rank, _)| *best_rank <= rank)
        {
            continue;
        }
        if let Some(input) = hwmon_package_input(&dir) {
            best = Some((rank, input));
        }
    }
    if best.is_none() {
        for dir in sorted_entries("/sys/class/thermal") {
            let Some(rank) = procfs::thermal_zone_rank(read_string(&dir.join("type")).trim())
            else {
                continue;
            };
            let temp = dir.join("temp");
            if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) && temp.exists() {
                best = Some((rank, temp));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn hwmon_package_input(dir: &Path) -> Option<PathBuf> {
    let inputs: Vec<(u32, Option<String>)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let index = procfs::temp_input_index(entry.file_name().to_str()?)?;
            let label = std::fs::read_to_string(dir.join(format!("temp{index}_label"))).ok();
            Some((index, label))
        })
        .collect();
    let index = procfs::pick_temp_input(&inputs)?;
    Some(dir.join(format!("temp{index}_input")))
}

/// The first DRM device with amdgpu's busy counter. Connector entries
/// (card0-DP-1) have no such file and are skipped.
fn find_gpu() -> Option<Gpu> {
    sorted_entries("/sys/class/drm")
        .into_iter()
        .find_map(|entry| {
            let dir = entry.join("device");
            let busy = File::open(dir.join("gpu_busy_percent")).ok()?;
            let runtime_status = File::open(dir.join("power/runtime_status")).ok();
            Some(Gpu {
                dir,
                busy,
                runtime_status,
            })
        })
}

/// System batteries' `uevent` files, skipping peripherals (`scope=Device`)
/// and, when any `BAT*` exists, anything else.
fn find_batteries(buffer: &mut [u8]) -> Vec<(String, File)> {
    let mut found = Vec::new();
    for dir in sorted_entries("/sys/class/power_supply") {
        let kind = read_string(&dir.join("type"));
        if kind.trim() != "Battery" {
            continue;
        }
        let (Some(name), Ok(uevent)) = (
            dir.file_name().and_then(|n| n.to_str()).map(str::to_string),
            File::open(dir.join("uevent")),
        ) else {
            continue;
        };
        let supply = read_attr(buffer, &uevent)
            .map(procfs::parse_uevent)
            .unwrap_or_default();
        if procfs::is_system_battery(&kind, &supply) {
            found.push((name, uevent));
        }
    }
    procfs::choose_batteries(found)
}

/// Interfaces backed by a device (see `procfs::pick_interfaces`).
fn physical_interfaces() -> Vec<String> {
    let all = sorted_entries("/sys/class/net")
        .into_iter()
        .filter_map(|dir| {
            let name = dir.file_name()?.to_str()?.to_string();
            Some((name, dir.join("device").exists()))
        });
    procfs::pick_interfaces(all)
}
