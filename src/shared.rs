//! Paths and wire format shared by the daemon and the bar client.
//!
//! Herdr runs the tab bar command outside any plugin context, so the bar client
//! cannot rely on `HERDR_PLUGIN_STATE_DIR`. Both binaries therefore resolve the
//! state directory the same way herdr does for plugins, and fall back to it when
//! the variable is absent.

use std::path::{Path, PathBuf};

pub const PLUGIN_ID: &str = "herdr-stats";

/// First word of every request. Bumped when the reply format changes, so an old
/// daemon left running across an upgrade is told to exit instead of being read.
pub const PROTOCOL: &str = "hs1";

/// Request asking the daemon for its health line instead of a frame.
pub const STATUS_REQUEST: &str = "?status";
/// Request asking the daemon to exit.
pub const STOP_REQUEST: &str = "!stop";

/// macOS limits `sun_path` to 104 bytes; Linux to 108. Stay under both.
const MAX_SOCKET_PATH: usize = 100;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Where herdr keeps this plugin's runtime state.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"));
    base.join("herdr/plugins").join(PLUGIN_ID)
}

/// Where herdr keeps this plugin's user config.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    herdr_config_dir().join("plugins/config").join(PLUGIN_ID)
}

/// Herdr's own config directory, honouring the same overrides herdr does.
pub fn herdr_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("herdr")
}

pub fn socket_path() -> PathBuf {
    let preferred = state_dir().join("daemon.sock");
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH {
        return preferred;
    }
    // A deep $HOME can push the state dir past sun_path. Fall back to a
    // directory only this user can write, so nobody else can put a socket
    // there first; if it is not ours, keep the long path and let bind fail.
    let dir = fallback_dir();
    let socket = dir.join("daemon.sock");
    let private = std::fs::symlink_metadata(&dir)
        .map(|meta| is_private_dir(&meta))
        .unwrap_or(true);
    if private && socket.as_os_str().len() <= MAX_SOCKET_PATH {
        socket
    } else {
        preferred
    }
}

/// Creates the fallback socket directory (mode 0700) when the socket lives
/// there. Only the daemon calls this, before it binds.
pub fn prepare_socket_dir(socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = fallback_dir();
    if socket.parent() != Some(dir.as_path()) {
        return Ok(());
    }
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&dir)?;
            if is_private_dir(&meta) {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "{} is not a private directory of ours",
                    dir.display()
                )))
            }
        }
        Err(err) => Err(err),
    }
}

fn fallback_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("herdr-stats-{uid}"))
}

fn is_private_dir(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.is_dir() && meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o077 == 0
}

pub fn lock_path() -> PathBuf {
    state_dir().join("daemon.lock")
}

pub fn log_path() -> PathBuf {
    state_dir().join("daemon.log")
}
