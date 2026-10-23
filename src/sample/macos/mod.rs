//! macOS samplers. Everything is read in-process through Mach, sysctl and
//! IOKit; nothing here shells out, and nothing needs root.

mod ffi;
mod smc;

use super::{Battery, Disk, Fan, Mem, Net, Pressure, Reading, TempWant, Temps};
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
};
use core_foundation_sys::number::{kCFNumberSInt32Type, CFNumberCreate};
use ffi::Owned;
use std::ffi::c_void;
use std::time::Instant;

const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
const CPU_STATE_MAX: usize = 4;
const CPU_STATE_IDLE: usize = 2;
const HOST_VM_INFO64: i32 = 4;
const HOST_VM_INFO64_COUNT: u32 = (std::mem::size_of::<VmStatistics64>() / 4) as u32;

const HID_TEMPERATURE_EVENT: i64 = 15;
const HID_TEMPERATURE_FIELD: i32 = 15 << 16;

pub struct Probe {
    host: ffi::mach_port_t,
    page_size: u64,
    mem_total: u64,
    prev_ticks: Vec<[u32; CPU_STATE_MAX]>,
    efficiency_cores: usize,
    pressure_mib: Option<Mib>,
    gpu: Option<Gpu>,
    net: NetCounter,
    thermal: Option<Thermal>,
    smc: Option<smc::Smc>,
}

impl Probe {
    pub fn new() -> Self {
        let mut probe = Self {
            host: unsafe { ffi::mach_host_self() },
            page_size: unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64,
            mem_total: sysctl_u64("hw.memsize").unwrap_or(0),
            prev_ticks: Vec::new(),
            efficiency_cores: sysctl_u64("hw.perflevel1.logicalcpu").unwrap_or(0) as usize,
            pressure_mib: Mib::new("kern.memorystatus_vm_pressure_level"),
            gpu: Gpu::open(),
            net: NetCounter::default(),
            thermal: None,
            smc: None,
        };
        // Prime the tick counters so the first real sample has a delta.
        let mut scratch = Reading::default();
        probe.cpu(&mut scratch);
        probe
    }

    pub fn cpu(&mut self, out: &mut Reading) {
        let mut count = 0u32;
        let mut info: *mut i32 = std::ptr::null_mut();
        let mut info_count = 0u32;
        let status = unsafe {
            ffi::host_processor_info(
                self.host,
                PROCESSOR_CPU_LOAD_INFO,
                &mut count,
                &mut info,
                &mut info_count,
            )
        };
        if status != ffi::KERN_SUCCESS || info.is_null() {
            return;
        }
        let raw = unsafe { std::slice::from_raw_parts(info, info_count as usize) };
        let cores = count as usize;
        let fresh = self.prev_ticks.len() != cores;
        if fresh {
            self.prev_ticks = vec![[0; CPU_STATE_MAX]; cores];
        }
        out.cores.clear();
        let (mut busy_sum, mut total_sum) = (0u64, 0u64);
        for core in 0..cores {
            let mut ticks = [0u32; CPU_STATE_MAX];
            for (state, tick) in ticks.iter_mut().enumerate() {
                *tick = raw[core * CPU_STATE_MAX + state] as u32;
            }
            let prev = self.prev_ticks[core];
            let delta: [u64; CPU_STATE_MAX] =
                std::array::from_fn(|s| ticks[s].wrapping_sub(prev[s]) as u64);
            let total: u64 = delta.iter().sum();
            let busy = total - delta[CPU_STATE_IDLE];
            busy_sum += busy;
            total_sum += total;
            out.cores.push(if total == 0 {
                0.0
            } else {
                busy as f32 / total as f32
            });
            self.prev_ticks[core] = ticks;
        }
        unsafe {
            ffi::vm_deallocate(
                ffi::task_self(),
                info as usize,
                info_count as usize * std::mem::size_of::<i32>(),
            );
        }
        if !fresh && total_sum > 0 {
            out.cpu = Some(busy_sum as f32 / total_sum as f32);
        }
    }

    pub fn memory(&mut self, out: &mut Reading) {
        let mut stats = VmStatistics64::default();
        let mut count = HOST_VM_INFO64_COUNT;
        let status = unsafe {
            ffi::host_statistics64(
                self.host,
                HOST_VM_INFO64,
                &mut stats as *mut VmStatistics64 as *mut i32,
                &mut count,
            )
        };
        if status != ffi::KERN_SUCCESS || self.mem_total == 0 {
            return;
        }
        // Activity Monitor's "Memory Used": app memory + wired + compressed,
        // where app memory is anonymous pages that cannot simply be dropped.
        let pages = |n: u32| n as u64 * self.page_size;
        let used = (pages(stats.active_count)
            + pages(stats.inactive_count)
            + pages(stats.speculative_count)
            + pages(stats.wire_count)
            + pages(stats.compressor_page_count))
        .saturating_sub(pages(stats.purgeable_count) + pages(stats.external_page_count));

        let pressure = match self.pressure_mib.as_ref().and_then(|mib| mib.read_i32()) {
            Some(4) => Pressure::Critical,
            Some(2) => Pressure::Warn,
            _ => Pressure::Normal,
        };
        out.mem = Some(Mem {
            used: used.min(self.mem_total),
            total: self.mem_total,
            pressure,
        });
    }

    pub fn gpu(&mut self, out: &mut Reading) {
        out.gpu = self.gpu.as_ref().and_then(Gpu::utilization);
    }

    pub fn network(&mut self, out: &mut Reading) {
        out.net = self.net.sample();
    }

    pub fn load(&mut self, out: &mut Reading) {
        let mut load = [0f64; 3];
        if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } > 0 {
            out.load1 = Some(load[0] as f32);
        }
    }

    pub fn thermal(&mut self, out: &mut Reading, want: TempWant) {
        let thermal = self.thermal.get_or_insert_with(Thermal::open);
        out.temp = thermal.read(want);
    }

    pub fn power(&mut self, out: &mut Reading, want_power: bool, want_fan: bool) {
        let Some(smc) = self.smc_handle() else {
            return;
        };
        if want_power {
            out.power_w = smc
                .read_f32(*b"PSTR")
                .filter(|w| w.is_finite() && *w >= 0.0);
        }
        if want_fan {
            out.fan = smc.fan();
        }
    }

    fn smc_handle(&mut self) -> Option<&mut smc::Smc> {
        if self.smc.is_none() {
            self.smc = smc::Smc::open();
        }
        self.smc.as_mut()
    }

    pub fn battery(&mut self, out: &mut Reading) {
        out.battery = read_battery();
    }

    pub fn disk(&mut self, out: &mut Reading, path: &str) {
        out.disk = read_disk(path);
    }

    /// Human-readable inventory for `herdr-stats doctor`.
    pub fn describe(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "cpu      {} logical cores ({} efficiency first)",
            self.prev_ticks.len(),
            self.efficiency_cores
        ));
        lines.push(format!(
            "memory   {:.1} GiB, page {} bytes",
            self.mem_total as f64 / GIB,
            self.page_size
        ));
        lines.push(format!(
            "gpu      {}",
            if self.gpu.is_some() {
                "IOAccelerator utilization"
            } else {
                "not available"
            }
        ));
        let thermal = self.thermal.get_or_insert_with(Thermal::open);
        lines.push(format!("thermal  {} HID sensors", thermal.sensors.len()));
        for group in [
            Group::Cpu,
            Group::Gpu,
            Group::Die,
            Group::Ssd,
            Group::Battery,
        ] {
            let names: Vec<&str> = thermal
                .sensors
                .iter()
                .filter(|sensor| sensor.group == group)
                .map(|sensor| sensor.name.as_str())
                .collect();
            if !names.is_empty() {
                lines.push(format!("  {:<8} {}", group.label(), names.join(" ")));
            }
        }
        match self.smc_handle() {
            Some(smc) => {
                let power = smc.read_f32(*b"PSTR");
                let fan = smc.fan();
                lines.push(format!(
                    "smc      power {}  fans {}",
                    power.map_or("n/a".into(), |w| format!("{w:.1} W")),
                    fan.map_or("none".into(), |f| format!("{:.0} rpm", f.rpm))
                ));
            }
            None => lines.push("smc      not available".into()),
        }
        lines
    }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

const _: () = assert!(std::mem::size_of::<VmStatistics64>() == 152);

/// A sysctl name resolved to its MIB once, so each read skips name parsing.
struct Mib(Vec<i32>);

impl Mib {
    fn new(name: &str) -> Option<Self> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut mib = [0i32; 12];
        let mut len = mib.len();
        let status = unsafe { libc::sysctlnametomib(name.as_ptr(), mib.as_mut_ptr(), &mut len) };
        (status == 0).then(|| Self(mib[..len].to_vec()))
    }

    fn read<T: Default + Copy>(&self) -> Option<T> {
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>();
        let status = unsafe {
            libc::sysctl(
                self.0.as_ptr() as *mut i32,
                self.0.len() as u32,
                &mut value as *mut T as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0).then_some(value)
    }

    fn read_i32(&self) -> Option<i32> {
        self.read::<i32>()
    }
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value = 0u64;
    let mut size = std::mem::size_of::<u64>();
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    // Some keys are 32-bit; sysctl tells us by shrinking `size`.
    Some(if size == 4 {
        value & 0xffff_ffff
    } else {
        value
    })
}

/// GPU busy percentage from the accelerator's performance statistics.
struct Gpu {
    entry: ffi::io_object_t,
    stats_key: Owned,
    busy_key: Owned,
}

impl Gpu {
    fn open() -> Option<Self> {
        let matching = unsafe { ffi::IOServiceMatching(c"IOAccelerator".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        let entry = unsafe { ffi::IOServiceGetMatchingService(ffi::MAIN_PORT_DEFAULT, matching) };
        let gpu = Self {
            entry,
            stats_key: ffi::cfstr("PerformanceStatistics"),
            busy_key: ffi::cfstr("Device Utilization %"),
        };
        (entry != 0 && gpu.utilization().is_some()).then_some(gpu)
    }

    fn utilization(&self) -> Option<f32> {
        let stats = Owned::new(unsafe {
            ffi::IORegistryEntryCreateCFProperty(
                self.entry,
                self.stats_key.as_ptr() as _,
                kCFAllocatorDefault,
                0,
            )
        })?;
        let busy = ffi::to_i64(ffi::dict_get(stats.as_ptr(), &self.busy_key))?;
        Some((busy as f32 / 100.0).clamp(0.0, 1.0))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe { ffi::IOObjectRelease(self.entry) };
    }
}

/// Bytes in and out across physical interfaces, from the 64-bit counters in
/// `NET_RT_IFLIST2` (the `getifaddrs` ones are 32-bit and wrap at 4 GiB).
#[derive(Default)]
struct NetCounter {
    buffer: Vec<u8>,
    names: std::collections::HashMap<u16, bool>,
    prev: Option<(Instant, u64, u64)>,
}

impl NetCounter {
    const RTM_IFINFO2: u8 = 0x12;
    const IFT_ETHER: u8 = 0x06;

    fn sample(&mut self) -> Option<Net> {
        let (rx, tx) = self.totals()?;
        let now = Instant::now();
        let rate = self.prev.and_then(|(then, prev_rx, prev_tx)| {
            let seconds = now.duration_since(then).as_secs_f64();
            (seconds > 0.05).then(|| Net {
                rx: rx.saturating_sub(prev_rx) as f64 / seconds,
                tx: tx.saturating_sub(prev_tx) as f64 / seconds,
            })
        });
        self.prev = Some((now, rx, tx));
        rate
    }

    fn totals(&mut self) -> Option<(u64, u64)> {
        let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
        let mut len = 0usize;
        let sized = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if sized != 0 {
            return None;
        }
        // Interfaces can appear between the two calls; leave headroom.
        len += 512;
        if self.buffer.len() < len {
            self.buffer.resize(len, 0);
        }
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                self.buffer.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return None;
        }
        let data = &self.buffer[..len];
        let names = &mut self.names;
        let (mut rx, mut tx) = (0u64, 0u64);
        let mut offset = 0usize;
        while offset + 4 <= data.len() {
            let msg_len = u16::from_ne_bytes([data[offset], data[offset + 1]]) as usize;
            if msg_len < 4 || offset + msg_len > data.len() {
                break;
            }
            let msg = &data[offset..offset + msg_len];
            // if_msghdr2: type at 3, flags at 8, index at 12, if_data64 at 32;
            // inside if_data64, type at 0 and the byte counters at 64 and 72.
            if msg[3] == Self::RTM_IFINFO2 && msg.len() >= 32 + 80 {
                let flags = i32::from_ne_bytes(msg[8..12].try_into().unwrap());
                let index = u16::from_ne_bytes(msg[12..14].try_into().unwrap());
                let kind = msg[32];
                let up = flags & libc::IFF_UP != 0 && flags & libc::IFF_LOOPBACK == 0;
                if up && kind == Self::IFT_ETHER && is_physical(names, index) {
                    rx += u64::from_ne_bytes(msg[96..104].try_into().unwrap());
                    tx += u64::from_ne_bytes(msg[104..112].try_into().unwrap());
                }
            }
            offset += msg_len;
        }
        Some((rx, tx))
    }
}

/// `en*` only: AirDrop (`awdl`, `llw`), bridges and Apple's internal `anpi`
/// links would double-count or add noise. Cached per interface index.
fn is_physical(names: &mut std::collections::HashMap<u16, bool>, index: u16) -> bool {
    *names.entry(index).or_insert_with(|| {
        let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
        let ptr = unsafe { libc::if_indextoname(index as u32, name.as_mut_ptr()) };
        !ptr.is_null()
            && unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_bytes()
                .starts_with(b"en")
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Cpu,
    Gpu,
    Die,
    Ssd,
    Battery,
}

impl Group {
    /// Apple's sensor names: `PMU TP0s` (CPU cluster), `PMU TP1g` (GPU
    /// cluster), `PMU tdie3` (SoC die), `NAND CH0 temp`, `gas gauge battery`.
    /// `tdev` and `tcal` are board references, not something to report.
    fn classify(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let bare = lower.strip_prefix("pmu ").unwrap_or(&lower).trim();
        if bare.contains("battery") || bare.starts_with("gas gauge") {
            return Some(Self::Battery);
        }
        if bare.contains("nand") || bare.contains("ssd") {
            return Some(Self::Ssd);
        }
        if bare.starts_with("tdie") {
            return Some(Self::Die);
        }
        if let Some(rest) = bare.strip_prefix("tp") {
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                return Some(if rest.ends_with('g') {
                    Self::Gpu
                } else {
                    Self::Cpu
                });
            }
        }
        if let Some(rest) = bare.strip_prefix("tg") {
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                return Some(Self::Gpu);
            }
        }
        None
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Die => "die",
            Self::Ssd => "ssd",
            Self::Battery => "battery",
        }
    }
}

struct Sensor {
    name: String,
    group: Group,
    service: Owned,
}

/// Temperatures from the AppleSensors HID page (usage page 0xff00, usage 5).
///
/// Each event copy is a synchronous round trip to the sensor, about a
/// millisecond apiece, and sensors in a group agree to within a degree. So
/// the wanted groups are read in full once, then one sensor per call in
/// rotation: about 1 ms per second, and a spike still shows within a call.
struct Thermal {
    _client: Option<Owned>,
    sensors: Vec<Sensor>,
    values: Vec<Option<f32>>,
    cursor: usize,
    primed: Option<TempWant>,
}

impl Thermal {
    fn open() -> Self {
        let empty = Self {
            _client: None,
            sensors: Vec::new(),
            values: Vec::new(),
            cursor: 0,
            primed: None,
        };
        let client =
            match Owned::new(unsafe { ffi::IOHIDEventSystemClientCreate(kCFAllocatorDefault) }) {
                Some(client) => client,
                None => return empty,
            };
        let matching = hid_matching(0xff00, 5);
        unsafe { ffi::IOHIDEventSystemClientSetMatching(client.as_ptr(), matching.as_ptr() as _) };
        let Some(services) =
            Owned::new(unsafe { ffi::IOHIDEventSystemClientCopyServices(client.as_ptr()) } as _)
        else {
            return Self {
                _client: Some(client),
                ..empty
            };
        };
        let product = ffi::cfstr("Product");
        let mut sensors = Vec::new();
        for service in ffi::array_items(services.as_ptr() as _) {
            let name = Owned::new(unsafe {
                ffi::IOHIDServiceClientCopyProperty(service, product.as_ptr() as _)
            })
            .and_then(|name| ffi::to_string(name.as_ptr()));
            let Some(name) = name else { continue };
            let Some(group) = Group::classify(&name) else {
                continue;
            };
            unsafe { core_foundation_sys::base::CFRetain(service) };
            sensors.push(Sensor {
                name,
                group,
                service: Owned(service),
            });
        }
        // Each name is published twice (once per PMU); one reading is enough.
        sensors.sort_by(|a, b| a.name.cmp(&b.name));
        sensors.dedup_by(|a, b| a.name == b.name);
        let values = vec![None; sensors.len()];
        Self {
            _client: Some(client),
            sensors,
            values,
            cursor: 0,
            primed: None,
        }
    }

    fn wanted(&self, want: TempWant) -> Vec<usize> {
        let pick = |groups: &[Group]| -> Vec<usize> {
            (0..self.sensors.len())
                .filter(|&i| groups.contains(&self.sensors[i].group))
                .collect()
        };
        let mut groups = Vec::new();
        if want.cpu {
            groups.push(Group::Cpu);
        }
        if want.gpu {
            groups.push(Group::Gpu);
        }
        if want.soc {
            groups.push(Group::Die);
        }
        let chosen = pick(&groups);
        // Chips without cluster sensors still have die sensors.
        if chosen.is_empty() && (want.cpu || want.gpu) {
            pick(&[Group::Die])
        } else {
            chosen
        }
    }

    fn refresh(&mut self, index: usize) {
        let service = self.sensors[index].service.as_ptr();
        let event = Owned::new(unsafe {
            ffi::IOHIDServiceClientCopyEvent(service, HID_TEMPERATURE_EVENT, 0, 0)
        });
        let value = event.map(|event| unsafe {
            ffi::IOHIDEventGetFloatValue(event.as_ptr(), HID_TEMPERATURE_FIELD) as f32
        });
        self.values[index] = value.filter(|v| (1.0..150.0).contains(v));
    }

    /// Hottest sensor per group: the hottest die is the one that throttles,
    /// and an average across sensors hides exactly that spike.
    fn read(&mut self, want: TempWant) -> Option<Temps> {
        let wanted = self.wanted(want);
        if wanted.is_empty() {
            return None;
        }
        if self.primed != Some(want) {
            for &index in &wanted {
                self.refresh(index);
            }
            self.primed = Some(want);
        } else {
            self.cursor = (self.cursor + 1) % wanted.len();
            self.refresh(wanted[self.cursor]);
        }
        let mut temps = Temps::default();
        let mut any = false;
        for &index in &wanted {
            let Some(value) = self.values[index] else {
                continue;
            };
            let slot = match self.sensors[index].group {
                Group::Cpu => &mut temps.cpu,
                Group::Gpu => &mut temps.gpu,
                Group::Die => &mut temps.soc,
                // Listed by `doctor`, never shown: not what "how hot is it" means.
                Group::Ssd | Group::Battery => continue,
            };
            *slot = Some(slot.map_or(value, |current: f32| current.max(value)));
            any = true;
        }
        any.then_some(temps)
    }
}

fn hid_matching(page: i32, usage: i32) -> Owned {
    unsafe {
        let page_num = CFNumberCreate(
            kCFAllocatorDefault,
            kCFNumberSInt32Type,
            &page as *const i32 as _,
        );
        let usage_num = CFNumberCreate(
            kCFAllocatorDefault,
            kCFNumberSInt32Type,
            &usage as *const i32 as _,
        );
        let page_key = ffi::cfstr("PrimaryUsagePage");
        let usage_key = ffi::cfstr("PrimaryUsage");
        let keys = [page_key.as_ptr(), usage_key.as_ptr()];
        let values = [page_num as *const c_void, usage_num as *const c_void];
        let dict = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        core_foundation_sys::base::CFRelease(page_num as _);
        core_foundation_sys::base::CFRelease(usage_num as _);
        Owned(dict as _)
    }
}

fn read_battery() -> Option<Battery> {
    let blob = Owned::new(unsafe { ffi::IOPSCopyPowerSourcesInfo() })?;
    let list = Owned::new(unsafe { ffi::IOPSCopyPowerSourcesList(blob.as_ptr()) } as _)?;
    let kind_key = ffi::cfstr("Type");
    let current_key = ffi::cfstr("Current Capacity");
    let max_key = ffi::cfstr("Max Capacity");
    let charging_key = ffi::cfstr("Is Charging");
    let state_key = ffi::cfstr("Power Source State");
    for source in ffi::array_items(list.as_ptr() as _) {
        let desc = unsafe { ffi::IOPSGetPowerSourceDescription(blob.as_ptr(), source) } as _;
        if ffi::to_string(ffi::dict_get(desc, &kind_key)).as_deref() != Some("InternalBattery") {
            continue;
        }
        let current = ffi::to_i64(ffi::dict_get(desc, &current_key))? as f32;
        let max = ffi::to_i64(ffi::dict_get(desc, &max_key))
            .unwrap_or(100)
            .max(1) as f32;
        return Some(Battery {
            fraction: (current / max).clamp(0.0, 1.0),
            charging: ffi::to_bool(ffi::dict_get(desc, &charging_key)).unwrap_or(false),
            on_ac: ffi::to_string(ffi::dict_get(desc, &state_key)).as_deref() == Some("AC Power"),
        });
    }
    None
}

/// Capacity the way Finder reports it: purgeable space (snapshots, caches the
/// system will evict on demand) counts as available. Falls back to `statfs`.
fn read_disk(path: &str) -> Option<Disk> {
    let finder = (|| {
        let url = Owned::new(unsafe {
            ffi::CFURLCreateFromFileSystemRepresentation(
                kCFAllocatorDefault,
                path.as_ptr(),
                path.len() as _,
                1,
            )
        })?;
        let read = |key| {
            let mut value: core_foundation_sys::base::CFTypeRef = std::ptr::null();
            let ok = unsafe {
                ffi::CFURLCopyResourcePropertyForKey(
                    url.as_ptr(),
                    key,
                    &mut value,
                    std::ptr::null_mut(),
                )
            };
            let value = Owned::new(value)?;
            (ok != 0).then(|| ffi::to_i64(value.as_ptr())).flatten()
        };
        let total = read(unsafe { ffi::kCFURLVolumeTotalCapacityKey })?;
        let available = read(unsafe { ffi::kCFURLVolumeAvailableCapacityForImportantUsageKey })?;
        (total > 0 && available >= 0).then(|| Disk {
            used: (total - available.min(total)) as u64,
            total: total as u64,
        })
    })();
    finder.or_else(|| super::statfs_disk(path))
}
