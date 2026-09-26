//! Settings › Shortcuts on Windows: the new keys are read by a keyboard hook, as Snip 'n' Clip's
//! capture prompt does.
//!
//! The page in the floating box cannot be trusted with this on Windows. It only hears keys when
//! the box has the keyboard - which breaks the never-take-the-foreground rule - and even then
//! Windows keeps some chords for itself: Alt+Space opens a window's system menu and never
//! reaches the page, so it could not be chosen at all (found 2026-09-26).
//!
//! While the prompt is open the hook swallows every key, so the chord he presses reaches no other
//! program either. Esc on its own cancels. So that a keyboard can never be left captured, the
//! prompt gives up by itself after `TIMEOUT` without a successful choice.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
};

pub const TIMEOUT: Duration = Duration::from_secs(15);

/// What the prompt heard: a shortcut in the same form the page used to send, or a cancel.
pub enum Heard {
    Keys(String),
    Cancel,
}

type Sink = Arc<dyn Fn(Heard) + Send + Sync>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
/// Modifiers currently held, as the hook has seen them - the keys it swallows never reach the
/// system's own key state.
static CTRL: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
static SHIFT: AtomicBool = AtomicBool::new(false);
static WIN: AtomicBool = AtomicBool::new(false);
/// Other keys held right now, so a held key's auto-repeat is not heard as a new chord.
static HELD: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
/// Set once the prompt has heard a chord or a cancel. Nothing more is heard until the app
/// answers: a refusal opens it again (`extend`), anything else ends the prompt. Without this a
/// held key's auto-repeat sent the same chord several times at once, and the saves raced each
/// other - one of them could leave every shortcut unregistered (found 2026-09-26).
static HEARD: AtomicBool = AtomicBool::new(false);
/// Milliseconds since `EPOCH` at which the prompt gives up; pushed back after each refusal.
static DEADLINE: AtomicU64 = AtomicU64::new(0);
static EPOCH: Mutex<Option<Instant>> = Mutex::new(None);
/// Which prompt is open, so a finished prompt's safety net never touches a newer one.
static PROMPT: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    EPOCH.lock().get_or_insert_with(Instant::now).elapsed().as_millis() as u64
}

/// After a refusal ("already in use"): listen again, with the full time again.
pub fn extend() {
    DEADLINE.store(now_ms() + TIMEOUT.as_millis() as u64, Ordering::Relaxed);
    HEARD.store(false, Ordering::Relaxed);
}

fn deliver(heard: Heard) {
    // Off the hook thread: a hook has to return at once, and the app registers shortcuts here.
    // Cloned out first: the sink may end the prompt, which clears `SINK` under the same lock.
    std::thread::spawn(move || {
        let sink = SINK.lock().clone();
        if let Some(sink) = sink {
            sink(heard);
        }
    });
}

/// The shortcut's key, in the names the page used (`KeyboardEvent.code` without its prefix),
/// which is what Tauri's shortcut parser reads. `None` for keys that cannot be a shortcut.
pub fn key_name(vk: u32) -> Option<String> {
    let name = match vk {
        0x41..=0x5A | 0x30..=0x39 => return Some(char::from(vk as u8).to_string()),
        0x70..=0x87 => return Some(format!("F{}", vk - 0x6F)),
        0x60..=0x69 => return Some(format!("Numpad{}", vk - 0x60)),
        0x20 => "Space",
        0x0D => "Enter",
        0x09 => "Tab",
        0x08 => "Backspace",
        0x2E => "Delete",
        0x2D => "Insert",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x25 => "ArrowLeft",
        0x26 => "ArrowUp",
        0x27 => "ArrowRight",
        0x28 => "ArrowDown",
        0xC0 => "Backquote",
        0xBD => "Minus",
        0xBB => "Equal",
        0xDB => "BracketLeft",
        0xDD => "BracketRight",
        0xDC => "Backslash",
        0xBA => "Semicolon",
        0xDE => "Quote",
        0xBC => "Comma",
        0xBE => "Period",
        0xBF => "Slash",
        _ => return None,
    };
    Some(name.to_string())
}

/// The chord as the rest of the app spells shortcuts: modifiers in the page's order, then the key.
pub fn accelerator(ctrl: bool, alt: bool, shift: bool, win: bool, key: &str) -> String {
    let mut parts = Vec::new();
    if alt {
        parts.push("Alt");
    }
    if ctrl {
        parts.push("Ctrl");
    }
    if win {
        parts.push("Super");
    }
    if shift {
        parts.push("Shift");
    }
    parts.push(key);
    parts.join("+")
}

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    // Keystrokes this app sends itself pass straight through.
    if info.flags.contains(LLKHF_INJECTED) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let message = wparam.0 as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    let vk = info.vkCode;
    let modifier = match vk {
        0x10 | 0xA0 | 0xA1 => Some(&SHIFT),
        0x11 | 0xA2 | 0xA3 => Some(&CTRL),
        0x12 | 0xA4 | 0xA5 => Some(&ALT),
        0x5B | 0x5C => Some(&WIN),
        _ => None,
    };
    if let Some(flag) = modifier {
        if down || up {
            flag.store(down, Ordering::Relaxed);
        }
    } else if up {
        HELD[(vk & 0xFF) as usize].store(false, Ordering::Relaxed);
    } else if down {
        let repeat = HELD[(vk & 0xFF) as usize].swap(true, Ordering::Relaxed);
        let (ctrl, alt, shift, win) = (
            CTRL.load(Ordering::Relaxed),
            ALT.load(Ordering::Relaxed),
            SHIFT.load(Ordering::Relaxed),
            WIN.load(Ordering::Relaxed),
        );
        let heard = if vk == 0x1B && !(ctrl || alt || shift || win) {
            Some(Heard::Cancel)
        } else {
            key_name(vk).map(|key| Heard::Keys(accelerator(ctrl, alt, shift, win, &key)))
        };
        // One fresh press, one answer at a time.
        if let Some(heard) = heard {
            if !repeat && !HEARD.swap(true, Ordering::Relaxed) {
                deliver(heard);
            }
        }
    }
    // Swallowed: the chord is for us, not for whatever is underneath.
    LRESULT(1)
}

/// The open prompt. Dropping it lets go of the keyboard.
pub struct Capture {
    thread: u32,
}

impl Drop for Capture {
    fn drop(&mut self) {
        *SINK.lock() = None;
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Start listening for the new shortcut. `sink` hears the chord, or a cancel - including the
/// automatic one when `TIMEOUT` passes.
pub fn start(sink: impl Fn(Heard) + Send + Sync + 'static) -> Option<Capture> {
    for flag in [&CTRL, &ALT, &SHIFT, &WIN].into_iter().chain(HELD.iter()) {
        flag.store(false, Ordering::Relaxed);
    }
    *SINK.lock() = Some(Arc::new(sink));
    extend();
    let prompt = PROMPT.fetch_add(1, Ordering::Relaxed) + 1;

    let (tx, rx) = std::sync::mpsc::channel::<Option<u32>>();
    std::thread::spawn(move || unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), None, 0) else {
            let _ = tx.send(None);
            return;
        };
        let thread = GetCurrentThreadId();
        let _ = tx.send(Some(thread));
        // The safety net: a prompt left open lets go of the keyboard by itself.
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            if SINK.lock().is_none() || PROMPT.load(Ordering::Relaxed) != prompt {
                break;
            }
            // Not while a chord is being saved: the answer to that ends or reopens the prompt.
            if now_ms() > DEADLINE.load(Ordering::Relaxed) && !HEARD.swap(true, Ordering::Relaxed) {
                deliver(Heard::Cancel);
                let _ = PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0));
                break;
            }
        });
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    let thread = rx.recv_timeout(Duration::from_millis(500)).ok().flatten()?;
    Some(Capture { thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_named_as_the_page_named_them() {
        assert_eq!(key_name(0x20).as_deref(), Some("Space"));
        assert_eq!(key_name(0x56).as_deref(), Some("V"));
        assert_eq!(key_name(0x35).as_deref(), Some("5"));
        assert_eq!(key_name(0x74).as_deref(), Some("F5"));
        assert_eq!(key_name(0x87).as_deref(), Some("F24"));
        assert_eq!(key_name(0x12), None, "Alt alone is not a shortcut");
    }

    #[test]
    fn alt_space_can_be_chosen() {
        assert_eq!(accelerator(false, true, false, false, "Space"), "Alt+Space");
        assert_eq!(accelerator(true, true, true, false, "V"), "Alt+Ctrl+Shift+V");
    }

    #[test]
    fn every_captured_shortcut_is_one_tauri_can_register() {
        use std::str::FromStr;
        for accel in ["Alt+Space", "Ctrl+Space", "Alt+Ctrl+V", "Alt+Ctrl+Shift+V", "Ctrl+F5", "Super+Shift+Numpad1", "Ctrl+Backquote", "Alt+ArrowUp"] {
            assert!(
                tauri_plugin_global_shortcut::Shortcut::from_str(accel).is_ok(),
                "{accel} should parse"
            );
        }
    }
}
