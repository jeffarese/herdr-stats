//! One reading of the machine, and the platform probes that fill it in.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod procfs;

#[cfg(target_os = "linux")]
pub use linux::Probe;
#[cfg(target_os = "macos")]
pub use macos::Probe;

#[derive(Clone, Debug, Default)]
pub struct Reading {
    /// Whole-machine CPU busy fraction, 0..=1.
    pub cpu: Option<f32>,
    /// Per logical core, in the kernel's order (efficiency cores first on
    /// Apple Silicon).
    pub cores: Vec<f32>,
    pub mem: Option<Mem>,
    pub disk: Option<Disk>,
    pub temp: Option<Temps>,
    pub gpu: Option<f32>,
    pub net: Option<Net>,
    pub power_w: Option<f32>,
    pub fan: Option<Fan>,
    pub battery: Option<Battery>,
    pub load1: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pressure {
    #[default]
    Normal,
    Warn,
    Critical,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mem {
    pub used: u64,
    pub total: u64,
    pub pressure: Pressure,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Disk {
    pub used: u64,
    pub total: u64,
}

/// Which temperature groups a layout shows, so only those sensors are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TempWant {
    pub cpu: bool,
    pub gpu: bool,
    pub soc: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Temps {
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
    pub soc: Option<f32>,
}

impl Temps {
    /// The compute temperature that matters most: the hottest of CPU, GPU and
    /// die sensors.
    pub fn hottest(&self) -> Option<f32> {
        [self.cpu, self.gpu, self.soc]
            .into_iter()
            .flatten()
            .reduce(f32::max)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Net {
    /// Bytes per second.
    pub rx: f64,
    pub tx: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Fan {
    pub rpm: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Battery {
    pub fraction: f32,
    pub charging: bool,
    pub on_ac: bool,
}

/// Plain `statfs`/`statvfs` capacity, used where nothing better exists.
pub(crate) fn statfs_disk(path: &str) -> Option<Disk> {
    let path = std::ffi::CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    let available = stat.f_bavail as u64 * block;
    (total > 0).then(|| Disk {
        used: total.saturating_sub(available),
        total,
    })
}
