//! Parsers for the procfs and sysfs text the Linux probe reads. Everything
//! here is pure (text in, values out) so it is tested on any platform with
//! fixtures copied from real machines; `linux.rs` keeps the file I/O.

use super::{Battery, Mem, Pressure};

// ---------------------------------------------------------------- /proc/stat

/// Busy and idle ticks (USER_HZ, 1/100 s) of one `cpu` line of /proc/stat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub idle: u64,
}

impl CpuTimes {
    /// The fields after the label: user nice system idle iowait irq softirq
    /// steal [guest guest_nice]. Guest time is already counted in user and
    /// nice, so only the first eight are summed; idle is idle + iowait.
    fn parse(fields: &str) -> Option<Self> {
        let (mut all, mut idle, mut count) = (0u64, 0u64, 0);
        for (index, field) in fields.split_ascii_whitespace().take(8).enumerate() {
            let value: u64 = field.parse().ok()?;
            all = all.saturating_add(value);
            if index == 3 || index == 4 {
                idle = idle.saturating_add(value);
            }
            count += 1;
        }
        (count >= 4).then_some(Self {
            busy: all.saturating_sub(idle),
            idle,
        })
    }

    /// Busy and total ticks since `prev`, or None if a counter went
    /// backwards, which CPU hotplug does to the idle sums.
    fn since(self, prev: Self) -> Option<(u64, u64)> {
        let busy = self.busy.checked_sub(prev.busy)?;
        let idle = self.idle.checked_sub(prev.idle)?;
        Some((busy, busy + idle))
    }
}

/// Reads the `cpu` lines at the top of /proc/stat: returns the aggregate
/// (`cpu  …`) and fills `cores` with `(N, times)` for each `cpuN  …`, in the
/// kernel's order. Offline cores have no line.
pub fn parse_stat(text: &str, cores: &mut Vec<(u32, CpuTimes)>) -> Option<CpuTimes> {
    cores.clear();
    let mut total = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            break;
        };
        // The label runs to the first space: empty for the aggregate line,
        // the core number otherwise.
        let (id, fields) = rest
            .split_once(|c: char| c.is_ascii_whitespace())
            .unwrap_or((rest, ""));
        let Some(times) = CpuTimes::parse(fields) else {
            continue;
        };
        if id.is_empty() {
            total = Some(times);
        } else if let Ok(id) = id.parse() {
            cores.push((id, times));
        }
    }
    total
}

/// CPU busy fractions from successive /proc/stat samples.
#[derive(Default)]
pub struct CpuCounter {
    total: Option<CpuTimes>,
    cores: Vec<(u32, CpuTimes)>,
    scratch: Vec<(u32, CpuTimes)>,
}

impl CpuCounter {
    /// Logical cores seen in the last sample.
    pub fn cores(&self) -> usize {
        self.cores.len()
    }

    /// Takes a new sample and returns the whole-machine busy fraction since
    /// the previous one, filling `per_core` (cleared first). None on the
    /// priming sample, when a counter went backwards, and when less than
    /// one tick per core has passed (no time to measure: the old baseline
    /// is kept so the next call has a real interval).
    pub fn update(&mut self, text: &str, per_core: &mut Vec<f32>) -> Option<f32> {
        per_core.clear();
        let now = parse_stat(text, &mut self.scratch)?;
        let delta = match self.total {
            None => None,
            Some(prev) => match now.since(prev) {
                Some((_, total)) if total < self.scratch.len().max(1) as u64 => return None,
                delta => delta,
            },
        };
        if delta.is_some() {
            // Cores are matched by number, not position, so one going
            // offline does not shift the rest; a new or reset core reads 0.
            for (id, times) in &self.scratch {
                let fraction = self
                    .cores
                    .binary_search_by_key(id, |(prev_id, _)| *prev_id)
                    .ok()
                    .and_then(|index| times.since(self.cores[index].1))
                    .and_then(|(busy, total)| busy_fraction(busy, total));
                per_core.push(fraction.unwrap_or(0.0));
            }
        }
        std::mem::swap(&mut self.cores, &mut self.scratch);
        self.total = Some(now);
        delta.and_then(|(busy, total)| busy_fraction(busy, total))
    }
}

fn busy_fraction(busy: u64, total: u64) -> Option<f32> {
    (total > 0).then(|| (busy as f32 / total as f32).clamp(0.0, 1.0))
}

// ------------------------------------------------------------- /proc/meminfo

/// Used memory is MemTotal − MemAvailable, the kernel's own estimate of what
/// could be handed out without swapping. Kernels before 3.14 lack
/// MemAvailable; there it is approximated from free, buffers and caches.
pub fn parse_meminfo(text: &str) -> Option<Mem> {
    let (mut total, mut available) = (None, None);
    let mut fallback = 0u64;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kib) = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match name {
            "MemTotal" => total = Some(kib),
            "MemAvailable" => available = Some(kib),
            "MemFree" | "Buffers" | "Cached" | "SReclaimable" => fallback += kib,
            _ => {}
        }
    }
    let total = total.filter(|t| *t > 0)? * 1024;
    let available = (available.unwrap_or(fallback) * 1024).min(total);
    let pressure = match available as f64 / total as f64 {
        a if a < 0.05 => Pressure::Critical,
        a if a < 0.12 => Pressure::Warn,
        _ => Pressure::Normal,
    };
    Some(Mem {
        used: total - available,
        total,
        pressure,
    })
}

// ------------------------------------------------------------- /proc/net/dev

/// `(interface, rx bytes, tx bytes)` for each interface line. Old kernels
/// print no space after the colon (`eth0:123…`), so split on it first.
pub fn parse_net_dev(text: &str) -> impl Iterator<Item = (&str, u64, u64)> {
    text.lines().filter_map(|line| {
        let (name, counters) = line.split_once(':')?;
        let mut fields = counters.split_ascii_whitespace();
        let rx = fields.next()?.parse().ok()?;
        // rx: bytes packets errs drop fifo frame compressed multicast; tx next.
        let tx = fields.nth(7)?.parse().ok()?;
        Some((name.trim(), rx, tx))
    })
}

/// Bytes between two readings of one counter. A drop is either a 32-bit
/// counter wrapping (32-bit kernels, some drivers) or a reset (driver reload,
/// interface re-created): it is taken as a wrap only when the counter fits in
/// 32 bits and the wrapped delta is under 2 GiB, otherwise as no traffic.
pub fn counter_delta(prev: u64, now: u64) -> u64 {
    if now >= prev {
        return now - prev;
    }
    if prev <= u64::from(u32::MAX) {
        let wrapped = now + (1u64 << 32) - prev;
        if wrapped < 1 << 31 {
            return wrapped;
        }
    }
    0
}

/// Which interfaces to count, from `(name, has a backing device)`. Devices
/// skip loopback, bridges, veths, tunnels and container networks, which would
/// double-count traffic; inside a container nothing has a device, so there
/// everything but loopback counts.
pub fn pick_interfaces(all: impl IntoIterator<Item = (String, bool)>) -> Vec<String> {
    let all: Vec<(String, bool)> = all.into_iter().collect();
    let any_device = all.iter().any(|(_, device)| *device);
    let mut names: Vec<String> = all
        .into_iter()
        .filter(|(name, device)| if any_device { *device } else { name != "lo" })
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
}

struct Iface {
    name: String,
    /// Counters at the previous sample, and in the one being read.
    last: Option<(u64, u64)>,
    now: Option<(u64, u64)>,
}

/// Traffic over a set of interfaces that can change between samples.
#[derive(Default)]
pub struct NetCounters {
    ifaces: Vec<Iface>,
}

impl NetCounters {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.ifaces.iter().map(|iface| iface.name.as_str())
    }

    /// Replaces the counted set, keeping the baselines of interfaces that
    /// stay, so a rescan does not disturb the rate.
    pub fn set_interfaces(&mut self, names: Vec<String>) {
        let mut old = std::mem::take(&mut self.ifaces);
        self.ifaces = names
            .into_iter()
            .map(|name| {
                let last = old
                    .iter_mut()
                    .find(|iface| iface.name == name)
                    .and_then(|iface| iface.last.take());
                Iface {
                    name,
                    last,
                    now: None,
                }
            })
            .collect();
    }

    /// Bytes received and sent since the previous call, summed over the
    /// interfaces present in both samples: one that just appeared only sets
    /// its baseline, and one that vanished loses it, so set changes never
    /// read as a burst.
    pub fn update(&mut self, text: &str) -> (u64, u64) {
        for iface in &mut self.ifaces {
            iface.now = None;
        }
        for (name, rx, tx) in parse_net_dev(text) {
            if let Some(iface) = self.ifaces.iter_mut().find(|iface| iface.name == name) {
                iface.now = Some((rx, tx));
            }
        }
        let (mut rx, mut tx) = (0, 0);
        for iface in &mut self.ifaces {
            if let (Some((prev_rx, prev_tx)), Some((now_rx, now_tx))) = (iface.last, iface.now) {
                rx += counter_delta(prev_rx, now_rx);
                tx += counter_delta(prev_tx, now_tx);
            }
            iface.last = iface.now;
        }
        (rx, tx)
    }
}

// ------------------------------------------------------------- temperatures

/// How well a hwmon chip's `name` stands for "the CPU temperature", lower is
/// better; None for chips that are something else (nvme, amdgpu, iwlwifi,
/// Super I/O, batteries). acpitz is a firmware zone that is often a fixed
/// or board value, so it only counts when nothing else exists.
pub fn hwmon_rank(name: &str) -> Option<u8> {
    match name {
        "k10temp" | "coretemp" | "zenpower" | "k8temp" => Some(0),
        "cpu_thermal" => Some(1),
        "acpitz" => Some(4),
        name if name.contains("cpu") => Some(2),
        name if name.contains("soc") => Some(3),
        _ => None,
    }
}

/// The same ranking for /sys/class/thermal zone `type`s, used when no hwmon
/// chip qualifies.
pub fn thermal_zone_rank(kind: &str) -> Option<u8> {
    match kind {
        "x86_pkg_temp" => Some(0),
        "acpitz" => Some(4),
        kind if kind.contains("cpu") => Some(1),
        kind if kind.contains("soc") => Some(2),
        _ => None,
    }
}

/// N from a hwmon `tempN_input` file name.
pub fn temp_input_index(file_name: &str) -> Option<u32> {
    file_name
        .strip_prefix("temp")?
        .strip_suffix("_input")?
        .parse()
        .ok()
}

/// Which of a chip's `tempN_input`s is the package temperature, from
/// `(N, tempN_label)`: Tdie (k10temp shows it only when Tctl carries an
/// offset), Package id 0 (coretemp), Tctl, else the lowest N.
pub fn pick_temp_input(inputs: &[(u32, Option<String>)]) -> Option<u32> {
    const LABELS: [&str; 3] = ["Tdie", "Package id 0", "Tctl"];
    LABELS
        .iter()
        .find_map(|want| {
            inputs
                .iter()
                .find(|(_, label)| label.as_deref().map(str::trim) == Some(*want))
        })
        .or_else(|| inputs.iter().min_by_key(|(index, _)| *index))
        .map(|(index, _)| *index)
}

/// °C from a sysfs millidegree reading, ignoring anything outside 1–150 °C
/// (0 from a sleeping sensor, −273 or garbage from a broken one).
pub fn millidegrees(text: &str) -> Option<f32> {
    let celsius = text.trim().parse::<i64>().ok()? as f32 / 1000.0;
    (1.0..150.0).contains(&celsius).then_some(celsius)
}

// ---------------------------------------------------------------------- GPU

/// amdgpu's `gpu_busy_percent`.
pub fn busy_percent(text: &str) -> Option<f32> {
    let pct: u32 = text.trim().parse().ok()?;
    (pct <= 100).then(|| pct as f32 / 100.0)
}

// ------------------------------------------------------------ power supplies

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Unknown,
    Charging,
    Discharging,
    NotCharging,
    Full,
}

/// One power supply's `uevent`. Units are sysfs's: % for capacity, µWh for
/// energy, µAh for charge, µW for power, µA for current, µV for voltage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Supply {
    /// `scope=Device`: the battery of a peripheral (mouse, headset, pad).
    pub device_scope: bool,
    pub present: bool,
    pub status: Status,
    pub capacity: Option<i64>,
    pub energy_now: Option<i64>,
    pub energy_full: Option<i64>,
    pub charge_now: Option<i64>,
    pub charge_full: Option<i64>,
    pub power_now: Option<i64>,
    pub current_now: Option<i64>,
    pub voltage_now: Option<i64>,
}

impl Default for Supply {
    fn default() -> Self {
        Self {
            device_scope: false,
            present: true,
            status: Status::Unknown,
            capacity: None,
            energy_now: None,
            energy_full: None,
            charge_now: None,
            charge_full: None,
            power_now: None,
            current_now: None,
            voltage_now: None,
        }
    }
}

pub fn parse_uevent(text: &str) -> Supply {
    let mut supply = Supply::default();
    for line in text.lines() {
        let Some((key, value)) = line
            .strip_prefix("POWER_SUPPLY_")
            .and_then(|l| l.split_once('='))
        else {
            continue;
        };
        let value = value.trim();
        let number = || value.parse::<i64>().ok();
        match key {
            "SCOPE" => supply.device_scope = value == "Device",
            "PRESENT" => supply.present = value != "0",
            "STATUS" => {
                supply.status = match value {
                    "Charging" => Status::Charging,
                    "Discharging" => Status::Discharging,
                    "Not charging" => Status::NotCharging,
                    "Full" => Status::Full,
                    _ => Status::Unknown,
                }
            }
            "CAPACITY" => supply.capacity = number(),
            "ENERGY_NOW" => supply.energy_now = number(),
            "ENERGY_FULL" => supply.energy_full = number(),
            "CHARGE_NOW" => supply.charge_now = number(),
            "CHARGE_FULL" => supply.charge_full = number(),
            "POWER_NOW" => supply.power_now = number(),
            "CURRENT_NOW" => supply.current_now = number(),
            "VOLTAGE_NOW" => supply.voltage_now = number(),
            _ => {}
        }
    }
    supply
}

impl Supply {
    fn fraction(&self) -> Option<f32> {
        let ratio = |now: Option<i64>, full: Option<i64>| {
            let full = full.filter(|f| *f > 0)?;
            Some(now? as f32 / full as f32)
        };
        self.capacity
            .map(|pct| pct as f32 / 100.0)
            .or_else(|| ratio(self.energy_now, self.energy_full))
            .or_else(|| ratio(self.charge_now, self.charge_full))
            .map(|f| f.clamp(0.0, 1.0))
    }

    /// Watts drawn from this battery, only while it is discharging: on AC
    /// `power_now` is the charge rate or nothing, not what the machine uses,
    /// so the power module shows battery draw and hides on AC. Batteries that
    /// report current instead of power give current × voltage; some drivers
    /// sign the current, so magnitudes are used.
    fn discharge_watts(&self) -> Option<f32> {
        if self.status != Status::Discharging {
            return None;
        }
        let watts = match (self.power_now, self.current_now, self.voltage_now) {
            (Some(micro_w), _, _) if micro_w != 0 => micro_w.unsigned_abs() as f64 / 1e6,
            (_, Some(micro_a), Some(micro_v)) => {
                micro_a.unsigned_abs() as f64 * micro_v.unsigned_abs() as f64 / 1e12
            }
            _ => return None,
        };
        (watts > 0.0).then_some(watts as f32)
    }
}

/// A battery that powers the machine: type Battery, not a peripheral's.
pub fn is_system_battery(kind: &str, supply: &Supply) -> bool {
    kind.trim() == "Battery" && !supply.device_scope
}

/// Orders found batteries by name and, when any is a `BAT*`, keeps only
/// those: the ACPI names are the laptop's own, other names may be
/// peripherals whose driver does not set a scope.
pub fn choose_batteries<T>(mut found: Vec<(String, T)>) -> Vec<(String, T)> {
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if found.iter().any(|(name, _)| name.starts_with("BAT")) {
        found.retain(|(name, _)| name.starts_with("BAT"));
    }
    found
}

/// One reading from all present system batteries (ThinkPads can have two):
/// charge weighted by energy when every battery reports it, else the mean.
pub fn combine_batteries(supplies: &[Supply]) -> Option<Battery> {
    let present: Vec<&Supply> = supplies.iter().filter(|s| s.present).collect();
    let fraction = match present.as_slice() {
        [] => return None,
        [one] => one.fraction()?,
        many => {
            let energy: Option<(i64, i64)> = many.iter().try_fold((0, 0), |(now, full), s| {
                Some((
                    now + s.energy_now?,
                    full + s.energy_full.filter(|f| *f > 0)?,
                ))
            });
            match energy {
                Some((now, full)) => (now as f32 / full as f32).clamp(0.0, 1.0),
                None => {
                    let fractions: Vec<f32> = many.iter().filter_map(|s| s.fraction()).collect();
                    if fractions.is_empty() {
                        return None;
                    }
                    fractions.iter().sum::<f32>() / fractions.len() as f32
                }
            }
        }
    };
    Some(Battery {
        fraction,
        charging: present.iter().any(|s| s.status == Status::Charging),
        on_ac: !present.iter().any(|s| s.status == Status::Discharging),
    })
}

/// Total discharge power over the batteries, None when none is discharging.
pub fn discharge_watts(supplies: &[Supply]) -> Option<f32> {
    supplies
        .iter()
        .filter(|s| s.present)
        .filter_map(Supply::discharge_watts)
        .reduce(|a, b| a + b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 4-thread laptop, 6.8 kernel, trimmed after `ctxt`.
    const STAT_A: &str = "\
cpu  4705 356 584 3699176 2329 0 117 0 0 0
cpu0 1393 112 159 924303 710 0 64 0 0 0
cpu1 1105 82 140 925128 564 0 21 0 0 0
cpu2 1151 79 147 924870 533 0 17 0 0 0
cpu3 1056 83 138 924875 522 0 15 0 0 0
intr 1462898 18 9 0 0 0 0 0 0 1 0 0 0 4 0 0 0 0 0 0
ctxt 2932839
";
    // One second later: 400 ticks across four cores, 100 of them busy.
    const STAT_B: &str = "\
cpu  4765 356 604 3699476 2329 0 137 0 0 0
cpu0 1453 112 179 924323 710 0 64 0 0 0
cpu1 1105 82 140 925228 564 0 21 0 0 0
cpu2 1151 79 147 924930 553 0 37 0 0 0
cpu3 1056 83 138 924975 522 0 15 0 0 0
intr 1463898 18 9 0 0 0 0 0 0 1 0 0 0 4 0 0 0 0 0 0
";

    #[test]
    fn stat_labels() {
        let mut cores = Vec::new();
        let total = parse_stat(STAT_A, &mut cores).expect("aggregate line");
        assert_eq!(total.idle, 3699176 + 2329);
        assert_eq!(total.busy, 4705 + 356 + 584 + 117);
        assert_eq!(cores.len(), 4);
        assert_eq!(cores[0].0, 0);
        assert_eq!(cores[3].0, 3);
        assert_eq!(cores[0].1.idle, 924303 + 710);
    }

    #[test]
    fn stat_old_kernel_fields() {
        // 2.6.0–2.6.10 print seven fields; 2.4 printed four.
        let mut cores = Vec::new();
        let total = parse_stat(
            "cpu  10 0 5 100\ncpu0 10 0 5 100 3 1 1\npage 1 2\n",
            &mut cores,
        );
        assert_eq!(
            total,
            Some(CpuTimes {
                busy: 15,
                idle: 100
            })
        );
        assert_eq!(
            cores[0].1,
            CpuTimes {
                busy: 17,
                idle: 103
            }
        );
    }

    #[test]
    fn cpu_fraction() {
        let mut cpu = CpuCounter::default();
        let mut cores = Vec::new();
        assert_eq!(cpu.update(STAT_A, &mut cores), None, "priming sample");
        assert!(cores.is_empty());
        let busy = cpu.update(STAT_B, &mut cores).expect("a delta");
        assert!((busy - 0.25).abs() < 1e-6, "{busy}");
        assert_eq!(cores.len(), 4);
        assert!((cores[0] - 0.8).abs() < 1e-6, "{}", cores[0]);
        assert_eq!(cores[1], 0.0);
        assert!((cores[2] - 0.2).abs() < 1e-6);
        assert_eq!(cpu.cores(), 4);
    }

    #[test]
    fn cpu_no_time_passed() {
        let mut cpu = CpuCounter::default();
        let mut cores = vec![0.5];
        cpu.update(STAT_A, &mut cores);
        assert_eq!(cpu.update(STAT_A, &mut cores), None);
        assert!(cores.is_empty());
        // The baseline was kept, so the next sample measures from STAT_A.
        let busy = cpu.update(STAT_B, &mut cores).expect("a delta");
        assert!((busy - 0.25).abs() < 1e-6);
    }

    #[test]
    fn cpu_hotplug() {
        let mut cpu = CpuCounter::default();
        let mut cores = Vec::new();
        cpu.update(STAT_A, &mut cores);
        // cpu1 went offline, and the aggregate idle dropped with it.
        let offline = "\
cpu  4765 356 604 2774000 1800 0 137 0 0 0
cpu0 1453 112 179 924323 710 0 64 0 0 0
cpu2 1151 79 147 924950 553 0 37 0 0 0
cpu3 1056 83 138 924975 522 0 15 0 0 0
";
        assert_eq!(
            cpu.update(offline, &mut cores),
            None,
            "counters went backwards"
        );
        let later = "\
cpu  4795 356 604 2774270 1800 0 137 0 0 0
cpu0 1483 112 179 924393 710 0 64 0 0 0
cpu2 1151 79 147 925050 553 0 37 0 0 0
cpu3 1056 83 138 925075 522 0 15 0 0 0
";
        let busy = cpu.update(later, &mut cores).expect("re-primed");
        assert!((busy - 0.1).abs() < 1e-6, "{busy}");
        assert_eq!(cores.len(), 3);
        assert!((cores[0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn cpu_core_back_online() {
        let mut cpu = CpuCounter::default();
        let mut cores = Vec::new();
        cpu.update("cpu  10 0 0 100\ncpu0 10 0 0 100\n", &mut cores);
        let busy = cpu.update(
            "cpu  20 0 0 110\ncpu0 15 0 0 105\ncpu1 5 0 0 5\n",
            &mut cores,
        );
        assert_eq!(busy, Some(0.5));
        assert_eq!(cores, vec![0.5, 0.0]);
    }

    const MEMINFO: &str = "\
MemTotal:       32562060 kB
MemFree:         6187344 kB
MemAvailable:   21540128 kB
Buffers:          710788 kB
Cached:         14100112 kB
SwapCached:            0 kB
Active:          7906448 kB
SReclaimable:     955616 kB
HugePages_Total:       0
Hugepagesize:       2048 kB
";

    #[test]
    fn meminfo() {
        let mem = parse_meminfo(MEMINFO).expect("mem");
        assert_eq!(mem.total, 32562060 * 1024);
        assert_eq!(mem.used, (32562060 - 21540128) * 1024);
        assert_eq!(mem.pressure, Pressure::Normal);
    }

    #[test]
    fn meminfo_without_available() {
        let old = "MemTotal: 1000 kB\nMemFree: 10 kB\nBuffers: 10 kB\nCached: 20 kB\n";
        let mem = parse_meminfo(old).expect("mem");
        assert_eq!(mem.used, 960 * 1024);
        assert_eq!(mem.pressure, Pressure::Critical);
        assert!(parse_meminfo("MemFree: 20 kB\n").is_none());
    }

    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 88430141  311744    0    0    0     0          0         0 88430141  311744    0    0    0     0       0          0
enp5s0: 9876543210 7340123    0  211    0     0          0     51234 1234567890 3120456    0    0    0     0       0          0
wlp4s0: 52341023   61234    0    0    0     0          0         0  4123456   31234    0    0    0     0       0          0
docker0:  123456    1234    0    0    0     0          0         0   654321    4321    0    0    0     0       0          0
vethab12cd:  654321    4321    0    0    0     0          0         0   123456    1234    0    0    0     0       0          0
";

    #[test]
    fn net_dev_lines() {
        let lines: Vec<_> = parse_net_dev(NET_DEV).collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[1], ("enp5s0", 9876543210, 1234567890));
        assert_eq!(lines[4], ("vethab12cd", 654321, 123456));
        // 2.6-era format, no space after the colon.
        let old: Vec<_> =
            parse_net_dev("  eth0:12345678901 0 0 0 0 0 0 0 42 0 0 0 0 0 0 0\n").collect();
        assert_eq!(old, vec![("eth0", 12345678901, 42)]);
    }

    #[test]
    fn net_set_changes() {
        let mut net = NetCounters::default();
        net.set_interfaces(vec!["enp5s0".into()]);
        assert_eq!(net.update(NET_DEV), (0, 0), "priming");
        let later = NET_DEV
            .replace("9876543210", "9876544210")
            .replace("52341023", "52341523");
        // wlp4s0 joins: it only sets a baseline, it does not add its total.
        net.set_interfaces(vec!["enp5s0".into(), "wlp4s0".into()]);
        assert_eq!(net.update(&later), (1000, 0));
        // enp5s0 is unplugged: its line is gone.
        let unplugged: String = later
            .lines()
            .filter(|l| !l.contains("enp5s0"))
            .map(|l| format!("{l}\n"))
            .collect();
        let unplugged = unplugged.replace("52341523", "52341623");
        assert_eq!(net.update(&unplugged), (100, 0));
        // It comes back with reset counters: a new baseline, no burst.
        let back = format!("{unplugged}enp5s0: 500 1 0 0 0 0 0 0 700 1 0 0 0 0 0 0\n");
        assert_eq!(net.update(&back), (0, 0));
        assert_eq!(net.names().collect::<Vec<_>>(), vec!["enp5s0", "wlp4s0"]);
    }

    #[test]
    fn counters() {
        assert_eq!(counter_delta(100, 150), 50);
        assert_eq!(counter_delta(u32::MAX as u64 - 9, 20), 30, "32-bit wrap");
        assert_eq!(counter_delta(5_000_000, 100), 0, "reset");
        assert_eq!(counter_delta(1 << 40, 100), 0, "64-bit counter reset");
    }

    #[test]
    fn interfaces() {
        let host = vec![
            ("lo".to_string(), false),
            ("wlp4s0".to_string(), true),
            ("docker0".to_string(), false),
            ("enp5s0".to_string(), true),
        ];
        assert_eq!(pick_interfaces(host), vec!["enp5s0", "wlp4s0"]);
        let container = vec![("lo".to_string(), false), ("eth0".to_string(), false)];
        assert_eq!(pick_interfaces(container), vec!["eth0"]);
    }

    #[test]
    fn chip_ranking() {
        let rank = |names: &[&'static str]| {
            names
                .iter()
                .copied()
                .min_by_key(|n| hwmon_rank(n).unwrap_or(u8::MAX))
        };
        assert_eq!(
            rank(&["acpitz", "nvme", "k10temp", "amdgpu"]),
            Some("k10temp")
        );
        assert_eq!(rank(&["acpitz", "coretemp", "iwlwifi_1"]), Some("coretemp"));
        assert_eq!(rank(&["rp1_adc", "cpu_thermal"]), Some("cpu_thermal"));
        assert_eq!(hwmon_rank("nvme"), None);
        assert_eq!(hwmon_rank("amdgpu"), None);
        assert_eq!(hwmon_rank("nct6798"), None);
        assert_eq!(hwmon_rank("BAT0"), None);
        assert!(hwmon_rank("acpitz") > hwmon_rank("soc_thermal"));
        assert_eq!(thermal_zone_rank("x86_pkg_temp"), Some(0));
        assert_eq!(thermal_zone_rank("iwlwifi_1"), None);
        assert_eq!(thermal_zone_rank("INT3400 Thermal"), None);
        assert!(thermal_zone_rank("cpu-thermal") < thermal_zone_rank("acpitz"));
    }

    #[test]
    fn temp_inputs() {
        assert_eq!(temp_input_index("temp3_input"), Some(3));
        assert_eq!(temp_input_index("temp3_label"), None);
        assert_eq!(temp_input_index("in0_input"), None);
        // k10temp on a Zen 2 Threadripper: Tctl carries an offset, Tdie not.
        let k10 = vec![
            (1, Some("Tctl\n".to_string())),
            (2, Some("Tdie\n".to_string())),
            (3, Some("Tccd1\n".to_string())),
        ];
        assert_eq!(pick_temp_input(&k10), Some(2));
        let zen3 = vec![
            (3, Some("Tccd1".to_string())),
            (1, Some("Tctl".to_string())),
        ];
        assert_eq!(pick_temp_input(&zen3), Some(1));
        let coretemp = vec![
            (2, Some("Core 0".to_string())),
            (1, Some("Package id 0".to_string())),
            (3, Some("Core 1".to_string())),
        ];
        assert_eq!(pick_temp_input(&coretemp), Some(1));
        assert_eq!(pick_temp_input(&[(2, None), (1, None)]), Some(1));
        assert_eq!(pick_temp_input(&[]), None);
    }

    #[test]
    fn temperature_range() {
        assert_eq!(millidegrees("45250\n"), Some(45.25));
        assert_eq!(millidegrees("0\n"), None);
        assert_eq!(millidegrees("-273150\n"), None);
        assert_eq!(millidegrees("255000\n"), None);
        assert_eq!(millidegrees("garbage"), None);
    }

    #[test]
    fn gpu_busy() {
        assert_eq!(busy_percent("37\n"), Some(0.37));
        assert_eq!(busy_percent("0\n"), Some(0.0));
        assert_eq!(busy_percent("255\n"), None);
        assert_eq!(busy_percent(""), None);
    }

    // ThinkPad T14, ACPI battery in power mode, on battery.
    const BAT0_DISCHARGING: &str = "\
POWER_SUPPLY_NAME=BAT0
POWER_SUPPLY_TYPE=Battery
POWER_SUPPLY_STATUS=Discharging
POWER_SUPPLY_PRESENT=1
POWER_SUPPLY_TECHNOLOGY=Li-poly
POWER_SUPPLY_CYCLE_COUNT=112
POWER_SUPPLY_VOLTAGE_MIN_DESIGN=15440000
POWER_SUPPLY_VOLTAGE_NOW=16312000
POWER_SUPPLY_POWER_NOW=7563000
POWER_SUPPLY_ENERGY_FULL_DESIGN=50500000
POWER_SUPPLY_ENERGY_FULL=46230000
POWER_SUPPLY_ENERGY_NOW=36980000
POWER_SUPPLY_CAPACITY=79
POWER_SUPPLY_CAPACITY_LEVEL=Normal
POWER_SUPPLY_MODEL_NAME=5B10W13930
POWER_SUPPLY_MANUFACTURER=SMP
POWER_SUPPLY_SERIAL_NUMBER= 1234
";
    // ACPI battery in current mode (charge_* and current_now), on battery.
    const BAT1_CURRENT: &str = "\
POWER_SUPPLY_NAME=BAT1
POWER_SUPPLY_TYPE=Battery
POWER_SUPPLY_STATUS=Discharging
POWER_SUPPLY_PRESENT=1
POWER_SUPPLY_VOLTAGE_NOW=11850000
POWER_SUPPLY_CURRENT_NOW=850000
POWER_SUPPLY_CHARGE_FULL_DESIGN=4400000
POWER_SUPPLY_CHARGE_FULL=4100000
POWER_SUPPLY_CHARGE_NOW=2050000
POWER_SUPPLY_CAPACITY=50
";
    // A Logitech mouse over HID++.
    const MOUSE: &str = "\
POWER_SUPPLY_NAME=hidpp_battery_0
POWER_SUPPLY_TYPE=Battery
POWER_SUPPLY_ONLINE=1
POWER_SUPPLY_STATUS=Discharging
POWER_SUPPLY_SCOPE=Device
POWER_SUPPLY_MODEL_NAME=MX Master 3
POWER_SUPPLY_MANUFACTURER=Logitech
POWER_SUPPLY_CAPACITY_LEVEL=Normal
POWER_SUPPLY_CAPACITY=55
";

    #[test]
    fn uevent() {
        let bat = parse_uevent(BAT0_DISCHARGING);
        assert_eq!(bat.status, Status::Discharging);
        assert_eq!(bat.capacity, Some(79));
        assert_eq!(bat.power_now, Some(7563000));
        assert!(bat.present && !bat.device_scope);
        assert!(parse_uevent(MOUSE).device_scope);
        assert_eq!(
            parse_uevent("POWER_SUPPLY_STATUS=Not charging\n").status,
            Status::NotCharging
        );
        assert!(!parse_uevent("POWER_SUPPLY_PRESENT=0\n").present);
    }

    #[test]
    fn battery_choice() {
        assert!(is_system_battery(
            "Battery\n",
            &parse_uevent(BAT0_DISCHARGING)
        ));
        assert!(!is_system_battery("Battery\n", &parse_uevent(MOUSE)));
        assert!(!is_system_battery("Mains\n", &Supply::default()));
        let found = vec![
            ("hid-00:1f-battery".to_string(), 1),
            ("BAT1".to_string(), 2),
            ("BAT0".to_string(), 3),
        ];
        let names: Vec<String> = choose_batteries(found)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["BAT0", "BAT1"]);
        let arm = vec![("axp20x-battery".to_string(), ())];
        assert_eq!(choose_batteries(arm).len(), 1);
    }

    #[test]
    fn battery_readings() {
        let one = combine_batteries(&[parse_uevent(BAT0_DISCHARGING)]).expect("battery");
        assert!((one.fraction - 0.79).abs() < 1e-6);
        assert!(!one.on_ac && !one.charging);

        let charging = BAT0_DISCHARGING.replace("Discharging", "Charging");
        let one = combine_batteries(&[parse_uevent(&charging)]).expect("battery");
        assert!(one.on_ac && one.charging);

        // ThinkPad charge threshold: on AC, holding at 80%.
        let held = BAT0_DISCHARGING.replace("Discharging", "Not charging");
        let one = combine_batteries(&[parse_uevent(&held)]).expect("battery");
        assert!(one.on_ac && !one.charging);

        // Two batteries, one in charge mode: mean of capacities.
        let two = combine_batteries(&[parse_uevent(BAT0_DISCHARGING), parse_uevent(BAT1_CURRENT)])
            .expect("battery");
        assert!((two.fraction - 0.645).abs() < 1e-6, "{}", two.fraction);

        // Two in energy mode, one empty bay: weighted by energy.
        let empty = parse_uevent("POWER_SUPPLY_PRESENT=0\nPOWER_SUPPLY_STATUS=Unknown\n");
        let second = parse_uevent(
            "POWER_SUPPLY_STATUS=Unknown\nPOWER_SUPPLY_ENERGY_NOW=3020000\nPOWER_SUPPLY_ENERGY_FULL=23770000\nPOWER_SUPPLY_CAPACITY=12\n",
        );
        let two =
            combine_batteries(&[parse_uevent(BAT0_DISCHARGING), second, empty]).expect("battery");
        assert!(
            (two.fraction - 40000000.0 / 70000000.0).abs() < 1e-6,
            "{}",
            two.fraction
        );
        assert!(!two.on_ac);

        assert!(combine_batteries(&[empty]).is_none());
        assert!(combine_batteries(&[]).is_none());
    }

    #[test]
    fn battery_power() {
        let watts = discharge_watts(&[parse_uevent(BAT0_DISCHARGING)]).expect("watts");
        assert!((watts - 7.563).abs() < 1e-4);
        // current × voltage: 0.85 A × 11.85 V.
        let watts = discharge_watts(&[parse_uevent(BAT1_CURRENT)]).expect("watts");
        assert!((watts - 10.0725).abs() < 1e-4, "{watts}");
        let signed = BAT1_CURRENT.replace("CURRENT_NOW=850000", "CURRENT_NOW=-850000");
        assert!(discharge_watts(&[parse_uevent(&signed)]).is_some());
        let both = discharge_watts(&[parse_uevent(BAT0_DISCHARGING), parse_uevent(BAT1_CURRENT)])
            .expect("watts");
        assert!((both - 17.6355).abs() < 1e-3);
        // On AC the module hides.
        let charging = BAT0_DISCHARGING.replace("Discharging", "Charging");
        assert_eq!(discharge_watts(&[parse_uevent(&charging)]), None);
        let idle = BAT0_DISCHARGING.replace("POWER_NOW=7563000", "POWER_NOW=0");
        assert_eq!(discharge_watts(&[parse_uevent(&idle)]), None);
    }
}
