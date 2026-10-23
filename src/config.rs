//! User config (`config.toml` in the plugin config dir) layered over a preset.
//!
//! Every field is optional. A preset supplies a complete layout; anything the
//! user sets replaces the preset's value, so `preset = "full"` plus
//! `[cpu] width = 12` is a full layout with a wider CPU graph.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const PRESETS: [&str; 4] = ["minimal", "compact", "balanced", "full"];
pub const DEFAULT_PRESET: &str = "balanced";

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct File {
    pub preset: Option<String>,
    pub modules: Option<Vec<String>>,
    pub icons: Option<String>,
    pub style: Option<String>,
    pub gap: Option<usize>,
    pub alerts: Option<String>,
    pub cpu: GraphFile,
    pub gpu: GraphFile,
    pub mem: MemFile,
    pub disk: DiskFile,
    pub temp: TempFile,
    pub net: NetFile,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphFile {
    pub graph: Option<String>,
    pub width: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemFile {
    pub gauge: Option<usize>,
    pub value: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskFile {
    pub path: Option<String>,
    pub gauge: Option<usize>,
    pub value: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TempFile {
    pub sensor: Option<String>,
    pub unit: Option<String>,
    pub warm: Option<f32>,
    pub hot: Option<f32>,
    pub critical: Option<f32>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetFile {
    pub graph: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icons {
    Nerd,
    Text,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Braille,
    Blocks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alerts {
    Icon,
    Blink,
    Emoji,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graph {
    History,
    Cores,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amount {
    UsedTotal,
    Used,
    Free,
    Percent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensor {
    Hottest,
    Cpu,
    Gpu,
    Soc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Module {
    Cpu {
        graph: Graph,
        width: usize,
    },
    Gpu {
        graph: Graph,
        width: usize,
    },
    Mem {
        gauge: usize,
        value: Amount,
    },
    Disk {
        gauge: usize,
        value: Amount,
    },
    Temp {
        sensor: Sensor,
        fahrenheit: bool,
        warm: f32,
        hot: f32,
        critical: f32,
    },
    Net {
        graph: usize,
    },
    Power,
    Fan,
    Battery,
    Load,
}

/// Which samplers a layout actually needs, so unused ones never run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    pub temp_want: crate::sample::TempWant,
    pub cpu: bool,
    pub gpu: bool,
    pub mem: bool,
    pub disk: bool,
    pub temp: bool,
    pub net: bool,
    pub power: bool,
    pub fan: bool,
    pub battery: bool,
    pub load: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub preset: String,
    pub modules: Vec<Module>,
    pub icons: Icons,
    pub style: Style,
    pub gap: usize,
    pub alerts: Alerts,
    pub disk_path: String,
}

impl Layout {
    pub fn needs(&self) -> Needs {
        let mut needs = Needs::default();
        for module in &self.modules {
            match module {
                Module::Cpu { .. } => needs.cpu = true,
                Module::Gpu { .. } => needs.gpu = true,
                Module::Mem { .. } => needs.mem = true,
                Module::Disk { .. } => needs.disk = true,
                Module::Temp { sensor, .. } => {
                    needs.temp = true;
                    let want = &mut needs.temp_want;
                    match sensor {
                        Sensor::Hottest => {
                            want.cpu = true;
                            want.gpu = true;
                        }
                        Sensor::Cpu => want.cpu = true,
                        Sensor::Gpu => want.gpu = true,
                        Sensor::Soc => want.soc = true,
                    }
                }
                Module::Net { .. } => needs.net = true,
                Module::Power => needs.power = true,
                Module::Fan => needs.fan = true,
                Module::Battery => needs.battery = true,
                Module::Load => needs.load = true,
            }
        }
        needs
    }
}

fn preset(name: &str) -> Option<File> {
    let graph = |graph: &str, width| GraphFile {
        graph: Some(graph.into()),
        width: Some(width),
    };
    let mem = |gauge, value: &str| MemFile {
        gauge: Some(gauge),
        value: Some(value.into()),
    };
    let disk = |gauge, value: &str| DiskFile {
        gauge: Some(gauge),
        value: Some(value.into()),
        path: None,
    };
    let modules = |list: &[&str]| Some(list.iter().map(|m| m.to_string()).collect());
    let base = File {
        icons: Some("nerd".into()),
        style: Some("braille".into()),
        gap: Some(2),
        alerts: Some("icon".into()),
        gpu: graph("history", 4),
        net: NetFile { graph: Some(0) },
        ..Default::default()
    };
    Some(match name {
        // Numbers only, for narrow windows or a busy tab row.
        "minimal" => File {
            modules: modules(&["cpu", "mem", "disk", "temp"]),
            icons: Some("none".into()),
            cpu: graph("history", 5),
            mem: mem(0, "used_total"),
            disk: disk(0, "percent"),
            ..base
        },
        "compact" => File {
            modules: modules(&["cpu", "mem", "disk", "temp"]),
            cpu: graph("history", 6),
            mem: mem(0, "used_total"),
            disk: disk(0, "percent"),
            ..base
        },
        "balanced" => File {
            modules: modules(&["cpu", "mem", "disk", "temp"]),
            cpu: graph("history", 8),
            mem: mem(3, "used_total"),
            disk: disk(3, "percent"),
            ..base
        },
        // Everything that fits in herdr's 80-character status limit.
        "full" => File {
            modules: modules(&["cpu", "gpu", "mem", "disk", "temp", "power", "net"]),
            cpu: graph("history", 6),
            gpu: graph("history", 3),
            mem: mem(3, "used_total"),
            disk: disk(0, "percent"),
            ..base
        },
        _ => return None,
    })
}

/// Resolves the user file over a preset. `override_preset` comes from the bar
/// command line and wins over the file's `preset`.
pub fn resolve(file: &File, override_preset: Option<&str>) -> Result<Layout, String> {
    let name = override_preset
        .filter(|name| !name.is_empty())
        .or(file.preset.as_deref())
        .unwrap_or(DEFAULT_PRESET);
    let base = preset(name)
        .ok_or_else(|| format!("unknown preset \"{name}\" (choose {})", PRESETS.join(", ")))?;
    let pick = |user: &Option<String>, preset: &Option<String>| -> String {
        user.clone().or_else(|| preset.clone()).unwrap_or_default()
    };

    let icons = match pick(&file.icons, &base.icons).as_str() {
        "nerd" => Icons::Nerd,
        "text" => Icons::Text,
        "none" => Icons::None,
        other => return Err(format!("icons = \"{other}\" (choose nerd, text, none)")),
    };
    let style = match pick(&file.style, &base.style).as_str() {
        "braille" => Style::Braille,
        "blocks" => Style::Blocks,
        other => return Err(format!("style = \"{other}\" (choose braille, blocks)")),
    };
    let alerts = match pick(&file.alerts, &base.alerts).as_str() {
        "icon" => Alerts::Icon,
        "blink" => Alerts::Blink,
        "emoji" => Alerts::Emoji,
        "off" => Alerts::Off,
        other => {
            return Err(format!(
                "alerts = \"{other}\" (choose icon, blink, emoji, off)"
            ))
        }
    };
    let gap = file.gap.or(base.gap).unwrap_or(2).min(8);

    let graph_of =
        |user: &GraphFile, preset: &GraphFile, what: &str| -> Result<(Graph, usize), String> {
            let graph = match pick(&user.graph, &preset.graph).as_str() {
                "history" | "" => Graph::History,
                "cores" => Graph::Cores,
                "none" => Graph::None,
                other => {
                    return Err(format!(
                        "[{what}] graph = \"{other}\" (choose history, cores, none)"
                    ))
                }
            };
            Ok((graph, user.width.or(preset.width).unwrap_or(8).min(40)))
        };
    let amount = |value: String, what: &str| -> Result<Amount, String> {
        Ok(match value.as_str() {
            "used_total" | "" => Amount::UsedTotal,
            "used" => Amount::Used,
            "free" => Amount::Free,
            "percent" | "pct" => Amount::Percent,
            other => {
                return Err(format!(
                    "[{what}] value = \"{other}\" (choose used_total, used, free, percent)"
                ))
            }
        })
    };

    let names = file
        .modules
        .clone()
        .or(base.modules.clone())
        .unwrap_or_default();
    let mut modules = Vec::with_capacity(names.len());
    for name in &names {
        modules.push(match name.as_str() {
            "cpu" => {
                let (graph, width) = graph_of(&file.cpu, &base.cpu, "cpu")?;
                Module::Cpu { graph, width }
            }
            "gpu" => {
                let (graph, width) = graph_of(&file.gpu, &base.gpu, "gpu")?;
                Module::Gpu { graph: if graph == Graph::Cores { Graph::History } else { graph }, width }
            }
            "mem" | "memory" | "ram" => Module::Mem {
                gauge: file.mem.gauge.or(base.mem.gauge).unwrap_or(0).min(20),
                value: amount(pick(&file.mem.value, &base.mem.value), "mem")?,
            },
            "disk" | "ssd" => Module::Disk {
                gauge: file.disk.gauge.or(base.disk.gauge).unwrap_or(0).min(20),
                value: amount(pick(&file.disk.value, &base.disk.value), "disk")?,
            },
            "temp" | "temperature" => {
                let sensor = match pick(&file.temp.sensor, &base.temp.sensor).as_str() {
                    "hottest" | "" => Sensor::Hottest,
                    "cpu" => Sensor::Cpu,
                    "gpu" => Sensor::Gpu,
                    "soc" | "die" => Sensor::Soc,
                    other => {
                        return Err(format!("[temp] sensor = \"{other}\" (choose hottest, cpu, gpu, soc)"))
                    }
                };
                let fahrenheit = match pick(&file.temp.unit, &base.temp.unit).as_str() {
                    "c" | "C" | "" => false,
                    "f" | "F" => true,
                    other => return Err(format!("[temp] unit = \"{other}\" (choose c, f)")),
                };
                Module::Temp {
                    sensor,
                    fahrenheit,
                    warm: file.temp.warm.unwrap_or(50.0),
                    hot: file.temp.hot.unwrap_or(80.0),
                    critical: file.temp.critical.unwrap_or(100.0),
                }
            }
            "net" | "network" => Module::Net {
                graph: file.net.graph.or(base.net.graph).unwrap_or(0).min(20),
            },
            "power" => Module::Power,
            "fan" => Module::Fan,
            "battery" => Module::Battery,
            "load" => Module::Load,
            other => {
                return Err(format!(
                    "unknown module \"{other}\" (choose cpu, gpu, mem, disk, temp, net, power, fan, battery, load)"
                ))
            }
        });
    }

    Ok(Layout {
        preset: name.to_string(),
        modules,
        icons,
        style,
        gap,
        alerts,
        disk_path: file.disk.path.clone().unwrap_or_else(|| "/".into()),
    })
}

pub fn path() -> PathBuf {
    crate::shared::config_dir().join("config.toml")
}

pub fn load(path: &Path) -> Result<File, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Written on first run so the knobs are discoverable without reading docs.
pub const TEMPLATE: &str = r#"# herdr-stats — the stats in the corner of herdr's tab bar.
# Edits apply within two seconds; no reload needed.
#
# Preview every preset in your terminal:   herdr-stats preview --presets

# minimal | compact | balanced | full
preset = "balanced"

# Override the preset's modules and their order (left to right). Available:
# cpu, gpu, mem, disk, temp, net, power, fan, battery, load
# modules = ["cpu", "mem", "disk", "temp"]

# icons = "nerd"    # nerd (needs a Nerd Font) | text | none
# style = "braille" # braille | blocks
# gap = 2           # spaces between modules
# alerts = "icon"   # icon | blink | emoji | off — what a critical reading does

# [cpu]
# graph = "history" # history (scrolling load) | cores (one bar per core) | none
# width = 8         # graph width in cells; braille fits two seconds per cell

# [mem]
# gauge = 3         # gauge width in cells, 0 hides it
# value = "used_total" # used_total | used | free | percent

# [disk]
# path = "/"
# gauge = 3
# value = "percent"

# [temp]
# sensor = "hottest" # hottest | cpu | gpu | soc
# unit = "c"         # c | f
# warm = 50          # the thermometer icon fills up at these (°C)
# hot = 80
# critical = 100     # and turns into an alert here

# [net]
# graph = 0         # cells of combined traffic history, 0 hides it
"#;
