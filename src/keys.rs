//! Single-key input for the popups: herdr popups forward every key, so "any key
//! closes" is the expected interaction. Ctrl-C keeps working (ISIG stays on).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

/// The terminal settings to put back, for the panic hook: the release build
/// aborts on panic, so `Drop` never runs.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);
static HOOK: Once = Once::new();

/// Puts stdin in non-canonical, no-echo mode until dropped.
pub struct RawInput {
    saved: libc::termios,
}

impl RawInput {
    pub fn enable() -> Option<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return None;
        }
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if let Ok(mut slot) = SAVED.lock() {
            *slot = Some(saved);
        }
        install_panic_hook();
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
        Some(Self { saved })
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
        if let Ok(mut slot) = SAVED.lock() {
            *slot = None;
        }
    }
}

/// Also restore a hidden cursor if something panics.
pub fn show_cursor_on_panic() {
    install_panic_hook();
}

fn install_panic_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(slot) = SAVED.try_lock() {
                if let Some(saved) = slot.as_ref() {
                    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
                }
            }
            let _ = std::io::stderr().write_all(b"\x1b[?25h");
            previous(info);
        }));
    });
}

/// Sets `flag` when any key arrives.
pub fn watch(flag: &'static AtomicBool) {
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        if std::io::stdin().read(&mut byte).is_ok() {
            flag.store(true, Ordering::SeqCst);
        }
    });
}

pub fn wait_for_any() {
    let _raw = RawInput::enable();
    let mut byte = [0u8; 1];
    let _ = std::io::stdin().read(&mut byte);
}
