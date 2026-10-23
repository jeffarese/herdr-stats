//! Adds (or removes) the tab bar entry in herdr's `config.toml`.
//!
//! The file is edited with `toml_edit`, so comments, ordering and formatting
//! survive. A timestamped backup is written next to it first, the new text
//! replaces the file atomically, and herdr is told to reload so the change
//! shows up without a restart.

use crate::shared;
use std::fs::{File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{Array, DocumentMut, InlineTable, Item, RawString, Table, TableLike, Value};

const MARKER: &str = "herdr-stats-bar";

/// The sampling daemon exits after 180 s without a tick, so a longer interval
/// would always find it cold, with empty graphs.
const MAX_INTERVAL: u64 = 60;
const DEFAULT_INTERVAL: u64 = 1;

pub struct Options {
    /// `None` keeps the interval of an existing entry (1 s for a new one).
    pub interval: Option<u64>,
    /// `None` keeps the command of an existing entry (no preset for a new one).
    pub preset: Option<String>,
    pub reload: bool,
}

pub fn herdr_config_path() -> PathBuf {
    std::env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| shared::herdr_config_dir().join("config.toml"))
}

/// Stable path herdr calls. GitHub installs live under a per-commit directory
/// that is cleaned up after updates, so the config points at a link in the
/// state dir instead, and `start` re-points it at the current install.
pub fn bar_link() -> PathBuf {
    shared::state_dir().join("bin").join(MARKER)
}

pub fn refresh_bar_link() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|err| format!("cannot locate own binary: {err}"))?;
    let target = exe.with_file_name(MARKER);
    if !target.exists() {
        return Err(format!(
            "{} is missing; build the plugin first",
            target.display()
        ));
    }
    let link = bar_link();
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
        return Ok(link);
    }
    // Swap atomically so a tick in flight never sees a missing link.
    let staged = link.with_extension("new");
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(&target, &staged)
        .and_then(|_| std::fs::rename(&staged, &link))
        .map_err(|err| format!("{}: {err}", link.display()))?;
    Ok(link)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Check the flags before touching anything. herdr runs the command through
/// `/bin/sh -lc` every tick, so only known preset names get in.
fn validate(options: &Options) -> Result<(), String> {
    if let Some(interval) = options.interval {
        if !(1..=MAX_INTERVAL).contains(&interval) {
            return Err(format!(
                "--interval must be 1 to {MAX_INTERVAL} seconds, not {interval} \
                 (the stats daemon stops after 3 minutes without a tick)"
            ));
        }
    }
    if let Some(preset) = &options.preset {
        if !crate::config::PRESETS.contains(&preset.as_str()) {
            return Err(format!(
                "unknown preset \"{preset}\" (choose {})",
                crate::config::PRESETS.join(", ")
            ));
        }
    }
    Ok(())
}

fn command_for(preset: Option<&str>, link: &Path) -> String {
    let mut command = shell_quote(&link.display().to_string());
    if let Some(preset) = preset {
        command.push_str(" --preset ");
        command.push_str(&shell_quote(preset));
    }
    command
}

/// Fill a fresh entry, inline (`{ ... }`) or `[[ui.tab_bar_right]]` alike.
fn fill_entry(entry: &mut dyn TableLike, options: &Options, link: &Path) {
    let interval = options.interval.unwrap_or(DEFAULT_INTERVAL);
    entry.insert("type", Item::Value("command".into()));
    entry.insert(
        "command",
        Item::Value(command_for(options.preset.as_deref(), link).into()),
    );
    entry.insert("interval_seconds", Item::Value((interval as i64).into()));
    entry.insert("timeout_seconds", Item::Value(2i64.into()));
}

/// Update our existing entry in place: only what a flag asked for changes, so
/// a hand-edited interval or preset survives a plain re-run.
fn update_entry(entry: &mut dyn TableLike, options: &Options, link: &Path) {
    if let Some(preset) = &options.preset {
        set_keep_decor(entry, "command", command_for(Some(preset), link).into());
    }
    if let Some(interval) = options.interval {
        set_keep_decor(entry, "interval_seconds", (interval as i64).into());
    }
}

/// Set `key`, keeping the spacing and comments around the old value.
fn set_keep_decor(entry: &mut dyn TableLike, key: &str, value: Value) {
    match entry.get_mut(key).and_then(Item::as_value_mut) {
        Some(slot) => {
            let decor = slot.decor().clone();
            *slot = value;
            *slot.decor_mut() = decor;
        }
        None => {
            entry.insert(key, Item::Value(value));
        }
    }
}

fn is_ours(entry: &dyn TableLike) -> bool {
    entry
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| command.contains(MARKER))
}

fn is_ours_inline(value: &Value) -> bool {
    value.as_inline_table().is_some_and(|table| is_ours(table))
}

/// Append to a `tab_bar_right = [ ... ]` array. The last entry is drawn
/// rightmost, in the corner of the tab bar, so that is where ours goes.
fn append_inline(array: &mut Array, ours: Value) {
    let raw = |text: Option<&RawString>| {
        text.and_then(RawString::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let trailing = raw(Some(array.trailing()));
    let last_suffix = raw(array.iter().last().and_then(|v| v.decor().suffix()));
    if !trailing.contains('\n') && !last_suffix.contains('\n') {
        array.push(ours);
        return;
    }
    // One entry per line, indented like the last one.
    let indent = array
        .iter()
        .last()
        .and_then(|v| v.decor().prefix())
        .and_then(RawString::as_str)
        .filter(|prefix| prefix.contains('\n'))
        .and_then(|prefix| prefix.rsplit('\n').next())
        .filter(|indent| indent.trim().is_empty())
        .unwrap_or("  ")
        .to_string();
    // A comment ending the last line sits in that value's suffix (no trailing
    // comma) or in the array's trailing text. Lead our entry with it so it
    // stays on the user's line instead of moving after ours.
    let mut carried = String::new();
    if last_suffix.contains('\n') {
        if let Some(last) = array.iter_mut().last() {
            last.decor_mut().set_suffix("");
        }
        carried.push_str(&last_suffix);
    }
    carried.push_str(&trailing);
    if !carried.ends_with('\n') {
        carried.push('\n');
    }
    carried.push_str(&indent);
    array.push_formatted(ours.decorated(carried, ""));
    array.set_trailing("\n");
}

/// The config text with our entry added or updated, and what was done.
fn add_entry(
    original: &str,
    options: &Options,
    link: &Path,
) -> Result<(String, &'static str), String> {
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|err| format!("does not parse: {err}"))?;
    let ui = doc
        .entry("ui")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or("`ui` is not a table")?;
    let action = match ui.get_mut("tab_bar_right") {
        None => {
            let mut entry = InlineTable::new();
            fill_entry(&mut entry, options, link);
            let mut array = Array::new();
            array.push_formatted(Value::InlineTable(entry).decorated("\n  ", ""));
            array.set_trailing_comma(true);
            array.set_trailing("\n");
            // `[ui.toast]` alone makes `ui` implicit; give it a header for our key.
            ui.set_implicit(false);
            ui.insert("tab_bar_right", Item::Value(Value::Array(array)));
            if let Some(mut key) = ui.key_mut("tab_bar_right") {
                key.leaf_decor_mut().set_prefix(
                    "# Live system stats (herdr-stats). `herdr-stats setup --remove` takes it out.\n",
                );
            }
            "added"
        }
        Some(Item::Value(Value::Array(array))) => {
            let existing = array.iter().position(is_ours_inline);
            match existing
                .and_then(|index| array.get_mut(index))
                .and_then(Value::as_inline_table_mut)
            {
                Some(entry) => {
                    update_entry(entry, options, link);
                    "updated"
                }
                None => {
                    let mut entry = InlineTable::new();
                    fill_entry(&mut entry, options, link);
                    append_inline(array, Value::InlineTable(entry));
                    "appended"
                }
            }
        }
        // `[[ui.tab_bar_right]]` blocks, the form herdr's docs use.
        Some(Item::ArrayOfTables(tables)) => {
            let existing = tables.iter().position(|t| is_ours(t));
            match existing.and_then(|index| tables.get_mut(index)) {
                Some(entry) => {
                    update_entry(entry, options, link);
                    "updated"
                }
                None => {
                    // A new table with no position is printed right after the
                    // last one, so it lands at the end of the bar.
                    let mut entry = Table::new();
                    fill_entry(&mut entry, options, link);
                    tables.push(entry);
                    "appended"
                }
            }
        }
        Some(_) => {
            return Err(
                "ui.tab_bar_right is neither an array nor [[ui.tab_bar_right]] tables".into(),
            )
        }
    };
    Ok((doc.to_string(), action))
}

enum Removal {
    Removed(String),
    Nothing(&'static str),
}

/// The config text with our entry taken out.
fn remove_entry(original: &str) -> Result<Removal, String> {
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|err| format!("does not parse: {err}"))?;
    let Some(ui) = doc.get_mut("ui").and_then(Item::as_table_mut) else {
        return Ok(Removal::Nothing("no [ui] table; nothing to remove"));
    };
    let (before, after) = match ui.get_mut("tab_bar_right") {
        Some(Item::Value(Value::Array(array))) => {
            let before = array.len();
            array.retain(|value| !is_ours_inline(value));
            (before, array.len())
        }
        Some(Item::ArrayOfTables(tables)) => {
            let before = tables.len();
            tables.retain(|table| !is_ours(table));
            (before, tables.len())
        }
        _ => return Ok(Removal::Nothing("no ui.tab_bar_right; nothing to remove")),
    };
    if before == after {
        return Ok(Removal::Nothing(
            "the herdr-stats entry is not in the tab bar",
        ));
    }
    if after == 0 {
        ui.remove("tab_bar_right");
    }
    Ok(Removal::Removed(doc.to_string()))
}

pub fn install(options: &Options) -> Result<String, String> {
    validate(options)?;
    let link = refresh_bar_link()?;
    let path = herdr_config_path();
    let original = read(&path)?;
    let (updated, action) =
        add_entry(&original, options, &link).map_err(|err| format!("{}: {err}", path.display()))?;
    if updated == original {
        return Ok(format!(
            "{} already has the herdr-stats entry",
            path.display()
        ));
    }
    write_with_backup(&path, &original, &updated)?;
    Ok(apply(
        format!("{action} the herdr-stats entry in {}", path.display()),
        options.reload,
    ))
}

pub fn remove(reload: bool) -> Result<String, String> {
    let path = herdr_config_path();
    let original = read(&path)?;
    let updated =
        match remove_entry(&original).map_err(|err| format!("{}: {err}", path.display()))? {
            Removal::Removed(updated) => updated,
            Removal::Nothing(message) => return Ok(message.into()),
        };
    write_with_backup(&path, &original, &updated)?;
    Ok(apply(
        format!("removed the herdr-stats entry from {}", path.display()),
        reload,
    ))
}

/// Reload herdr after a write and say how that went: a failed reload must not
/// read like success, since the tab bar only changes once herdr reloads.
fn apply(written: String, reload: bool) -> String {
    const HINT: &str = "run `herdr server reload-config` to apply it";
    if !reload {
        return format!("{written}; {HINT}");
    }
    match reload_herdr() {
        Ok(()) => format!("{written} and reloaded herdr"),
        Err(reason) => format!("{written}, but herdr could not be reloaded ({reason}); {HINT}"),
    }
}

/// Whether the config already carries our entry, for `doctor`.
pub fn installed_command() -> Option<String> {
    let text = std::fs::read_to_string(herdr_config_path()).ok()?;
    let doc: DocumentMut = text.parse().ok()?;
    match doc.get("ui")?.get("tab_bar_right")? {
        Item::Value(Value::Array(entries)) => entries
            .iter()
            .find(|v| is_ours_inline(v))
            .map(|v| v.to_string().trim().to_string()),
        Item::ArrayOfTables(tables) => tables.iter().find(|t| is_ours(*t)).map(|table| {
            // One line, like the inline form.
            let mut inline = table.clone().into_inline_table();
            inline.fmt();
            inline.to_string().trim().to_string()
        }),
        _ => None,
    }
}

fn read(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn write_with_backup(path: &Path, original: &str, updated: &str) -> Result<(), String> {
    let target = resolve_symlinks(path)?;
    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    if !original.is_empty() {
        backup(path, original, permissions.clone())?;
    }
    replace_file(&target, updated, permissions)
}

/// Follow `path` through any symlinks to the file that holds the text, even
/// when that file does not exist yet. config.toml is often a symlink into a
/// dotfiles repo, and the link must stay a link.
fn resolve_symlinks(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let next = std::fs::read_link(&current)
                    .map_err(|err| format!("{}: {err}", current.display()))?;
                current = match current.parent() {
                    Some(dir) if next.is_relative() => dir.join(next),
                    _ => next,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(format!("{}: too many levels of symlinks", path.display()))
}

/// Create `dir/<name><n>` for the first free `n`, so two runs never share a
/// file. Starts private; the caller widens permissions once it is written.
fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, File), String> {
    for n in 0..1000 {
        let candidate = dir.join(if n == 0 {
            name.to_string()
        } else {
            format!("{name}-{n}")
        });
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("{}: {err}", candidate.display())),
        }
    }
    Err(format!("{}: no free name for {name}", dir.display()))
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "config.toml".into(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Copy the old text to `<name>.bak-herdr-stats-<unix seconds>[-n]` next to
/// `path`, with the original's permissions.
fn backup(
    path: &Path,
    original: &str,
    permissions: Option<Permissions>,
) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = path.parent().unwrap_or(Path::new(""));
    let (backup, mut file) =
        create_unique(dir, &format!("{}.bak-herdr-stats-{stamp}", file_name(path)))?;
    let written = file
        .write_all(original.as_bytes())
        .and_then(|_| match permissions {
            Some(permissions) => file.set_permissions(permissions),
            None => Ok(()),
        });
    written.map_err(|err| format!("{}: {err}", backup.display()))?;
    Ok(backup)
}

/// Write to a temporary file beside `target` and rename it over, so a crash or
/// a full disk leaves the old config whole rather than truncated.
fn replace_file(
    target: &Path,
    contents: &str,
    permissions: Option<Permissions>,
) -> Result<(), String> {
    let dir = target.parent().unwrap_or(Path::new(""));
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let name = format!(
        ".{}.herdr-stats-{}.tmp",
        file_name(target),
        std::process::id()
    );
    let (temp, mut file) = create_unique(dir, &name)?;
    // A new config gets the usual 0644; an existing one keeps its mode.
    let permissions =
        permissions.unwrap_or_else(|| std::os::unix::fs::PermissionsExt::from_mode(0o644));
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|_| file.set_permissions(permissions))
        .and_then(|_| file.sync_all())
        .and_then(|_| std::fs::rename(&temp, target));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("{}: {err}", target.display()));
    }
    // Make the rename itself durable; not fatal if the directory refuses.
    if let Ok(dir) = File::open(if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    }) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Ask herdr to reload its config; on failure, the reason in a few words.
fn reload_herdr() -> Result<(), String> {
    let output = Command::new(herdr_bin())
        .args(["server", "reload-config"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr.lines().map(str::trim).find(|line| !line.is_empty());
    Err(reason.map_or_else(|| output.status.to_string(), str::to_string))
}

/// The herdr binary to call. HERDR_BIN_PATH can outlive the binary it names
/// (upgrading herdr under a running server), so fall back to PATH.
pub fn herdr_bin() -> PathBuf {
    std::env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("herdr"))
}

/// Open one of the manifest's popup panes. Manifest commands run without a
/// shell, so the actions call this instead of the herdr CLI directly.
pub fn open_pane(entrypoint: &str) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = Command::new(herdr_bin())
        .args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            crate::shared::PLUGIN_ID,
        ])
        .args(["--entrypoint", entrypoint, "--placement", "popup"])
        .exec();
    eprintln!("herdr-stats: could not run herdr: {error}");
    2
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const LINK: &str = "/x/herdr-stats-bar";

    fn options(interval: Option<u64>, preset: Option<&str>) -> Options {
        Options {
            interval,
            preset: preset.map(str::to_string),
            reload: false,
        }
    }

    fn edit(original: &str) -> String {
        edit_with(original, &options(None, None)).0
    }

    fn edit_with(original: &str, options: &Options) -> (String, &'static str) {
        add_entry(original, options, Path::new(LINK)).unwrap()
    }

    fn entries(text: &str) -> Vec<toml::Value> {
        let parsed: toml::Table = toml::from_str(text).unwrap();
        parsed["ui"]["tab_bar_right"].as_array().unwrap().clone()
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("herdr-stats-setup-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn adds_entry_inside_existing_ui_table_and_keeps_comments() {
        let original = "# top\n[ui]\n# keep me\nstatus_indicators = \"symbols\"\n\n[ui.toast]\ndelivery = \"terminal\"\n";
        let updated = edit(original);
        assert!(updated.contains("# keep me"));
        let ui_at = updated.find("[ui]").unwrap();
        let entry_at = updated.find("tab_bar_right").unwrap();
        let toast_at = updated.find("[ui.toast]").unwrap();
        assert!(ui_at < entry_at && entry_at < toast_at, "{updated}");
        assert!(updated.contains("'/x/herdr-stats-bar'"));
    }

    #[test]
    fn creates_ui_header_when_only_subtables_exist() {
        let updated = edit("[ui.toast]\ndelivery = \"terminal\"\n");
        let parsed: toml::Table = toml::from_str(&updated).unwrap();
        assert!(parsed["ui"]["tab_bar_right"].is_array(), "{updated}");
        assert_eq!(parsed["ui"]["toast"]["delivery"].as_str(), Some("terminal"));
    }

    #[test]
    fn new_entry_defaults_to_one_second_and_no_preset() {
        let updated = edit("");
        let entry = &entries(&updated)[0];
        assert_eq!(entry["interval_seconds"].as_integer(), Some(1));
        assert_eq!(entry["command"].as_str(), Some("'/x/herdr-stats-bar'"));
    }

    #[test]
    fn quoting_survives_odd_paths() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn preset_must_be_known_and_is_quoted() {
        let err = validate(&options(None, Some("full; echo pwned"))).unwrap_err();
        assert!(
            err.contains("full; echo pwned") && err.contains("minimal, compact, balanced, full"),
            "{err}"
        );
        assert!(validate(&options(None, Some(""))).is_err());
        for preset in crate::config::PRESETS {
            assert!(validate(&options(None, Some(preset))).is_ok(), "{preset}");
        }
        assert_eq!(
            command_for(Some("full"), Path::new("/a b/herdr-stats-bar")),
            "'/a b/herdr-stats-bar' --preset 'full'"
        );
    }

    #[test]
    fn interval_must_be_one_to_sixty() {
        assert!(validate(&options(Some(0), None)).is_err());
        assert!(validate(&options(Some(61), None)).is_err());
        assert!(validate(&options(Some(3600), None))
            .unwrap_err()
            .contains("1 to 60"));
        assert!(validate(&options(Some(1), None)).is_ok());
        assert!(validate(&options(Some(60), None)).is_ok());
        assert!(validate(&options(None, None)).is_ok());
    }

    #[test]
    fn rerun_keeps_hand_edited_interval_and_preset() {
        let original = "[ui]\ntab_bar_right = [\n  { type = \"command\", command = \"'/x/herdr-stats-bar' --preset 'full'\", interval_seconds = 5, timeout_seconds = 2 },\n]\n";
        let (updated, _) = edit_with(original, &options(None, None));
        assert_eq!(updated, original, "a plain re-run changes nothing");

        let (updated, action) = edit_with(original, &options(Some(2), None));
        assert_eq!(action, "updated");
        let entry = &entries(&updated)[0];
        assert_eq!(entry["interval_seconds"].as_integer(), Some(2));
        assert!(
            entry["command"]
                .as_str()
                .unwrap()
                .ends_with("--preset 'full'"),
            "{updated}"
        );

        let (updated, _) = edit_with(original, &options(None, Some("minimal")));
        let entry = &entries(&updated)[0];
        assert_eq!(entry["interval_seconds"].as_integer(), Some(5));
        assert_eq!(
            entry["command"].as_str(),
            Some("'/x/herdr-stats-bar' --preset 'minimal'")
        );
        assert_eq!(entries(&updated).len(), 1);
    }

    #[test]
    fn appends_after_existing_entries_and_keeps_their_comment() {
        let original = "[ui]\ntab_bar_right = [\n  { type = \"command\", command = \"date\" }, # the clock\n]\n";
        let (updated, action) = edit_with(original, &options(None, None));
        assert_eq!(action, "appended");
        assert!(
            updated.contains("\"date\" }, # the clock\n  {"),
            "{updated}"
        );
        let list = entries(&updated);
        assert_eq!(list.len(), 2);
        assert!(list[1]["command"].as_str().unwrap().contains(MARKER));

        // Same without a trailing comma: the comment is the value's suffix.
        let original = "[ui]\ntab_bar_right = [\n  { type = \"command\", command = \"date\" } # the clock\n]\n";
        let (updated, _) = edit_with(original, &options(None, None));
        assert!(
            updated.contains("\"date\" }, # the clock\n  {"),
            "{updated}"
        );
        assert_eq!(entries(&updated).len(), 2);
    }

    const TABLES: &str = "[ui]\nstatus_indicators = \"symbols\"\n\n# the clock\n[[ui.tab_bar_right]]\ntype = \"command\"\ncommand = \"date\"\n\n[ui.toast]\ndelivery = \"terminal\"\n";

    #[test]
    fn installs_into_array_of_tables() {
        let (updated, action) = edit_with(TABLES, &options(None, Some("full")));
        assert_eq!(action, "appended");
        assert!(
            updated.contains("# the clock\n[[ui.tab_bar_right]]"),
            "{updated}"
        );
        let list = entries(&updated);
        assert_eq!(list.len(), 2, "{updated}");
        assert_eq!(list[0]["command"].as_str(), Some("date"));
        assert_eq!(
            list[1]["command"].as_str(),
            Some("'/x/herdr-stats-bar' --preset 'full'")
        );
        assert_eq!(list[1]["interval_seconds"].as_integer(), Some(1));
        let parsed: toml::Table = toml::from_str(&updated).unwrap();
        assert_eq!(parsed["ui"]["toast"]["delivery"].as_str(), Some("terminal"));

        // Re-running replaces ours in place rather than adding another.
        let (again, action) = edit_with(&updated, &options(Some(3), None));
        assert_eq!(action, "updated");
        let list = entries(&again);
        assert_eq!(list.len(), 2, "{again}");
        assert_eq!(list[1]["interval_seconds"].as_integer(), Some(3));
        assert!(list[1]["command"]
            .as_str()
            .unwrap()
            .ends_with("--preset 'full'"));
        let (same, _) = edit_with(&again, &options(None, None));
        assert_eq!(same, again);
    }

    #[test]
    fn removes_from_array_of_tables() {
        let (installed, _) = edit_with(TABLES, &options(None, None));
        let Removal::Removed(removed) = remove_entry(&installed).unwrap() else {
            panic!("not removed")
        };
        assert_eq!(removed, TABLES);
        assert!(matches!(remove_entry(TABLES).unwrap(), Removal::Nothing(_)));

        // Ours alone: the key goes with it.
        let alone =
            "[[ui.tab_bar_right]]\ntype = \"command\"\ncommand = \"'/x/herdr-stats-bar'\"\n";
        let Removal::Removed(removed) = remove_entry(alone).unwrap() else {
            panic!("not removed")
        };
        assert!(!removed.contains("tab_bar_right"), "{removed}");
    }

    #[test]
    fn removes_from_inline_array() {
        let installed = edit("[ui]\nfoo = 1\n");
        let Removal::Removed(removed) = remove_entry(&installed).unwrap() else {
            panic!("not removed")
        };
        assert!(
            !removed.contains("tab_bar_right") && removed.contains("foo = 1"),
            "{removed}"
        );
    }

    #[test]
    fn symlinked_config_stays_a_symlink() {
        let scratch = Scratch::new("symlink");
        let dotfiles = scratch.0.join("dotfiles");
        let config_dir = scratch.0.join("config");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        let target = dotfiles.join("herdr.toml");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o600)).unwrap();
        let link = config_dir.join("config.toml");
        // Relative, as dotfile managers often make them.
        std::os::unix::fs::symlink("../dotfiles/herdr.toml", &link).unwrap();

        write_with_backup(&link, "old\n", "new\n").unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(mode(&target), 0o600);
        // No temp file left beside the target; the backup sits beside the link.
        let leftovers: Vec<_> = std::fs::read_dir(&dotfiles)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        let backups: Vec<_> = std::fs::read_dir(&config_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".bak-herdr-stats-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), "old\n");
    }

    #[test]
    fn backups_never_overwrite_each_other() {
        let scratch = Scratch::new("backups");
        let path = scratch.0.join("config.toml");
        std::fs::write(&path, "one\n").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let permissions = std::fs::metadata(&path).ok().map(|m| m.permissions());
        let first = backup(&path, "one\n", permissions.clone()).unwrap();
        let second = backup(&path, "two\n", permissions).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two\n");
        assert_eq!(mode(&first), 0o640);
        assert_eq!(mode(&second), 0o640);
    }

    #[test]
    fn writes_new_config_and_keeps_mode_of_existing() {
        let scratch = Scratch::new("write");
        let path = scratch.0.join("herdr").join("config.toml");
        write_with_backup(&path, "", "first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        assert_eq!(mode(&path), 0o644);
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        write_with_backup(&path, "first\n", "second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        assert_eq!(mode(&path), 0o600);
        // The config and one backup, no temp files.
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
    }
}
