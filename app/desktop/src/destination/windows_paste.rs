//! The paste rung on Windows: the twin of `macos_paste`, for apps with no silent write path -
//! Electron (VS Code, Slack, Discord, the Claude and ChatGPT apps), Chrome without the extension,
//! Windows Terminal, Word.
//!
//! A paste lands in *whatever is focused*, so it is gated: **the same foreground window and the
//! same focused control as at the keypress**. If either changed it refuses, and the words wait on
//! the clipboard. Clicks and key presses are counted too; since 2026-09-29 they are forgiven inside
//! that window and control where something can say where the caret went (`SameWindow`), as on
//! the Mac: always in Chrome and Electron apps, and in a native app only when UI Automation sees
//! the same box.
//!
//! Windows keeps no system-wide input counters the way macOS does, so clicks and key presses are
//! counted by low-level input hooks - **only while a dictation is in flight**. The hooks are
//! installed at the keypress and removed when the last `FocusStamp` of that dictation is dropped.
//! They count; they record nothing about which keys were pressed.
//!
//! **Exactly one key may be pressed between the keypress and delivery: the one that stopped the
//! dictation.** Modifiers are not counted, and neither is the auto-repeat of the dictation
//! shortcut's own key still held from the start - that key only. Any other key - an arrow, a
//! letter, Backspace, Enter - may have moved the caret, and refuses the paste; so does any other
//! key already held when the dictation starts (its auto-repeat would move the caret unseen), and
//! so does no key at all, which means the count was not running. (Codex's reviews, 2026-09-26: the
//! first version allowed three keys; the second exempted every key held at the start.)
//!
//! What remains unseen: a key pressed *and* released in the few milliseconds between the shortcut
//! and the hooks going in. Keeping the hooks resident would close that too, at the price of a
//! permanent keyboard hook - which Snip 'n' Clip also declined.
//!
//! The paste is instant, even with the shortcut's keys still held: see `paste_keys`.
//!
//! Ctrl+V only ever reads the normal clipboard. On the normal-clipboard setting it already holds
//! the transcript (`complete_transcription` copies before it delivers). On Huck's own clipboard
//! the words are put on the normal one for half a second and whatever he had there is put back.

use crate::win_hook::{self, HookThread};
use hvtt_core::pipeline::{DeliveryError, Destination, Liveness};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_LWIN, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
    GetWindowThreadProcessId, GUITHREADINFO, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
};

const VK_V: u16 = 0x56;

// ---------------------------------------------------------------------------- input counting

static CLICKS: AtomicU32 = AtomicU32::new(0);
static KEYS: AtomicU32 = AtomicU32::new(0);
/// Keys currently held, so auto-repeat is not counted as typing.
static HELD: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// The virtual key of the dictation shortcut's own key (Space for Alt+Space); 0 if unknown.
static TRIGGER: AtomicU32 = AtomicU32::new(0);

/// The dictation shortcut's modifiers, one bit a kind (`modifier_bit`).
static TRIGGER_MODIFIERS: AtomicU32 = AtomicU32::new(0);

/// Which kind of modifier a key is - Ctrl 1, Alt 2, Shift 4, Windows 8 - or 0 for any other key.
fn modifier_bit(vk: u32) -> u32 {
    match vk {
        0x11 | 0xA2 | 0xA3 => 1,
        0x12 | 0xA4 | 0xA5 => 2,
        0x10 | 0xA0 | 0xA1 => 4,
        0x5B | 0x5C => 8,
        _ => 0,
    }
}

/// The modifiers an accelerator names (`Ctrl+Alt+Space`), as `modifier_bit`s.
fn modifiers_of(accelerator: &str) -> u32 {
    let mut parts: Vec<&str> = accelerator.split('+').map(str::trim).collect();
    parts.pop();
    parts.iter().fold(0, |bits, part| {
        bits | match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmdorctrl" | "commandorcontrol" => 1,
            "alt" | "option" => 2,
            "shift" => 4,
            "super" | "cmd" | "command" | "meta" => 8,
            _ => 0,
        }
    })
}

// ---------------------------------------------------------------------------- hold to talk
//
// Dictation Style > Hold to Talk: he holds the shortcut, talks, and lets go. Two things about a
// held shortcut need the keyboard hook that already counts for the gate.
//
// 1. Its own key must not type. Windows keeps repeating the last key pressed. While every key of
//    Alt+Space is down those repeats are the shortcut again and reach nobody; the moment he lets
//    go of Alt first, they are plain Spaces, typed into his text box for as long as his thumb is
//    still down. So from the start of a held dictation until that key comes up, its presses are
//    swallowed here. (Only presses: the release goes through, so Windows and the shortcut's own
//    watcher still see the key come up.)
// 2. Letting go of ANY of its keys is letting go. The shortcut library reports a release only
//    when the main key comes up; Alt up, Space still down, would otherwise keep recording.

/// The held dictation whose keys are being watched, by a number of its own; 0 for none. One of
/// its shortcut's keys coming up ends it - once.
static HOLDING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HOLD_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// That hold's own keys, as they were when it began - not the shortcut as it is now, which he
/// can change while a key is still down (Codex's review, 2026-10-05).
static HOLD_KEY: AtomicU32 = AtomicU32::new(0);
static HOLD_MODIFIERS: AtomicU32 = AtomicU32::new(0);
/// The key whose presses reach no program - a held dictation's own, until it comes up - with
/// the number of the hold it is for (`swallowing`); 0 for none. An earlier hold's tidying, late,
/// must not undo a newer hold's (Codex's second review).
static SWALLOWING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Keeps the hooks in after the dictation is over, until that key is up - with the number of
/// the hold it is kept for, so an old hold's tidying never takes a newer one's.
static HOLD_WATCH: Mutex<Option<(u64, Arc<Watch>)>> = Mutex::new(None);
/// How the program is told that a key of a held shortcut came up, and of which hold
/// (`on_hold_key_up`).
static HOLD_KEY_UP: std::sync::OnceLock<Box<dyn Fn(u64) + Send + Sync>> = std::sync::OnceLock::new();

/// Set once, when the program starts. `told` is given the hold's number (`hold_started`).
pub fn on_hold_key_up(told: impl Fn(u64) + Send + Sync + 'static) {
    let _ = HOLD_KEY_UP.set(Box::new(told));
}

/// A swallowed key and the hold it belongs to, as one number.
fn swallowing(serial: u64, vk: u32) -> u64 {
    (serial << 8) | (vk as u64 & 0xFF)
}

fn key_down(vk: u32) -> bool {
    vk != 0 && unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Is every kind of modifier in `wanted` held - on either side of the keyboard?
fn modifiers_down(wanted: u32) -> bool {
    let pairs: [(u32, [u32; 2]); 4] = [(1, [0xA2, 0xA3]), (2, [0xA4, 0xA5]), (4, [0xA0, 0xA1]), (8, [0x5B, 0x5C])];
    pairs.iter().all(|(bit, keys)| wanted & bit == 0 || keys.iter().any(|k| key_down(*k)))
}

/// Can the dictation shortcut's keys be watched at all? If not, a dictation is not held.
pub fn hold_key_known() -> bool {
    TRIGGER.load(Ordering::Relaxed) != 0
}

/// A held dictation has started, at the keypress: its number, by which a key coming up is
/// reported (`on_hold_key_up`); 0 if the shortcut's key is not known. From here its own key
/// types nothing, and any key of its shortcut coming up is told to the program.
///
/// The typing needs the keyboard hook - its own, if the gate has none running. If Windows will
/// not install one, a modifier let go is still noticed, by looking (the thread below); only
/// the swallowing is lost, and the held key then repeats into whatever has the keyboard, as any
/// held key does. Said in the log; nothing more is possible without the hook.
pub fn hold_started() -> u64 {
    let trigger = TRIGGER.load(Ordering::Relaxed);
    if trigger == 0 {
        return 0;
    }
    let modifiers = TRIGGER_MODIFIERS.load(Ordering::Relaxed);
    let serial = HOLD_SERIAL.fetch_add(1, Ordering::SeqCst) + 1;
    let watch = watch();
    let hooked = watch.is_some();
    if !hooked {
        eprintln!("[hvtt] hold to talk without the keyboard hook: the held key is not kept from other programs");
    }
    *HOLD_WATCH.lock() = watch.map(|w| (serial, w));
    HOLD_KEY.store(trigger, Ordering::SeqCst);
    HOLD_MODIFIERS.store(modifiers, Ordering::SeqCst);
    let mine = swallowing(serial, trigger);
    SWALLOWING.store(if hooked && key_down(trigger) { mine } else { 0 }, Ordering::SeqCst);
    HOLDING.store(serial, Ordering::SeqCst);
    // Looks, as well as the hook's listening: a modifier let go ends the hold even with no
    // hook. And once the key is up - which can be after the dictation is over - the hooks may
    // go. Everything here is this hold's own: a newer hold's is left alone.
    std::thread::spawn(move || {
        while key_down(trigger) {
            if !modifiers_down(modifiers) {
                hold_key_came_up(serial);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        hold_key_came_up(serial);
        let _ = SWALLOWING.compare_exchange(mine, 0, Ordering::SeqCst, Ordering::SeqCst);
        let kept = {
            let mut kept = HOLD_WATCH.lock();
            if kept.as_ref().is_some_and(|(of, _)| *of == serial) { kept.take() } else { None }
        };
        // Outside the lock: dropping the last one stops the hook thread.
        drop(kept);
    });
    serial
}

/// Asked once the recording exists: has a key of the held shortcut already come up - in the
/// moment the microphone took to open, or before the hook was in? The caller ends the dictation
/// itself; a report of the same thing, if one was also made, is then one too many and ignored.
///
/// `serial`: the hold's number from `hold_started`. With none (0) its keys were never noted -
/// the ones on record are an earlier hold's - so nothing is concluded from them.
pub fn hold_already_let_go(serial: u64) -> bool {
    serial != 0
        && !(key_down(HOLD_KEY.load(Ordering::SeqCst)) && modifiers_down(HOLD_MODIFIERS.load(Ordering::SeqCst)))
}

/// One of hold number `serial`'s keys came up: say so, once, naming the hold.
fn hold_key_came_up(serial: u64) {
    if serial != 0 && HOLDING.compare_exchange(serial, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        if let Some(told) = HOLD_KEY_UP.get() {
            told(serial);
        }
    }
}

/// Should the hook keep this key event from every program? Only a press, made by him, of the key
/// a held dictation is still holding down (`swallowing`; 0 for none).
fn swallowed(vk: u32, down: bool, injected: bool, swallowing: u64) -> bool {
    down && !injected && swallowing != 0 && vk as u64 == swallowing & 0xFF
}

/// Tell the gate which key starts and stops dictation, from its accelerator (`Alt+Space`).
pub fn set_trigger_key(accelerator: &str) {
    TRIGGER_MODIFIERS.store(modifiers_of(accelerator), Ordering::Relaxed);
    let key = accelerator.rsplit('+').next().unwrap_or("").trim();
    let key = key.strip_prefix("Key").or_else(|| key.strip_prefix("Digit")).unwrap_or(key);
    let vk = (0..=255u32)
        .find(|vk| crate::win_shortcut::key_name(*vk).is_some_and(|n| n.eq_ignore_ascii_case(key)))
        .unwrap_or(0);
    TRIGGER.store(vk, Ordering::Relaxed);
}

/// Mouse buttons also answer GetAsyncKeyState; they are the mouse hook's business, not typing.
fn is_mouse_button(vk: u32) -> bool {
    matches!(vk, 0x01 | 0x02 | 0x04 | 0x05 | 0x06)
}

/// Is any key other than modifiers, mouse buttons and the dictation shortcut's own key held down
/// right now?
fn other_key_held() -> bool {
    let trigger = TRIGGER.load(Ordering::Relaxed);
    (1..=255u32).any(|vk| {
        !is_modifier(vk)
            && !is_mouse_button(vk)
            && vk != trigger
            && unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    })
}

/// The running hook thread, shared by every stamp of the current dictation.
static WATCH: Mutex<Weak<Watch>> = Mutex::new(Weak::new());

/// While one of these is alive, clicks and key presses are being counted.
struct Watch {
    hooks: HookThread,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Watch({})", self.hooks.thread())
    }
}

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// Does this key press count as him doing something? Our own keystrokes (injected), a held key's
/// auto-repeat and modifiers on their own do not.
fn counts_as_typing(vk: u32, injected: bool, repeat: bool) -> bool {
    !injected && !repeat && !is_modifier(vk)
}

/// The input half of the gate, from what the hooks counted since the keypress and what the gate
/// expects: the clicks and keys he made inside Huck's box, plus the stop press when the shortcut
/// ended the dictation (`box_input`).
fn input_moved(clicks: u32, keys: u32, expected: (u32, u32)) -> Option<&'static str> {
    let (box_clicks, expected_keys) = expected;
    if clicks != box_clicks {
        return Some("clicked");
    }
    match keys.cmp(&expected_keys) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some("stop-not-seen"),
        std::cmp::Ordering::Greater => Some("typed"),
    }
}

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let vk = (info.vkCode & 0xFF) as usize;
        let message = wparam.0 as u32;
        let injected = info.flags.contains(LLKHF_INJECTED);
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        // Hold to Talk: the held key types nothing, and any of the shortcut's keys coming up
        // ends the dictation (see "hold to talk" above).
        let swallowing = SWALLOWING.load(Ordering::SeqCst);
        if swallowed(vk as u32, down, injected, swallowing) {
            return LRESULT(1);
        }
        if !down && !injected {
            // Up, so its next press is a fresh one and goes through - at once, not when the
            // tidying thread next looks. Only the entry just read: a newer hold's stays.
            if swallowing != 0 && vk as u64 == swallowing & 0xFF {
                let _ = SWALLOWING.compare_exchange(swallowing, 0, Ordering::SeqCst, Ordering::SeqCst);
            }
            let holding = HOLDING.load(Ordering::SeqCst);
            if holding != 0 {
                let mine = vk as u32 == HOLD_KEY.load(Ordering::SeqCst)
                    || modifier_bit(vk as u32) & HOLD_MODIFIERS.load(Ordering::SeqCst) != 0;
                if mine {
                    hold_key_came_up(holding);
                }
            }
        }
        if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
            let repeat = !injected && HELD[vk].swap(true, Ordering::Relaxed);
            if counts_as_typing(vk as u32, injected, repeat) {
                KEYS.fetch_add(1, Ordering::Relaxed);
            }
        } else if !injected && (message == WM_KEYUP || message == WM_SYSKEYUP) {
            HELD[vk].store(false, Ordering::Relaxed);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn on_mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let message = wparam.0 as u32;
        let press = [WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_MBUTTONDOWN, WM_XBUTTONDOWN];
        if press.contains(&message) && info.flags & LLMHF_INJECTED == 0 {
            CLICKS.fetch_add(1, Ordering::Relaxed);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Runs on the hook thread with the hooks in. Only the dictation shortcut's own key, if it is
/// still under his fingers, is marked held, so its auto-repeat is not counted as typing. Any other
/// key is marked up: its next repeat counts, like a fresh press.
fn mark_held_keys() {
    let trigger = TRIGGER.load(Ordering::Relaxed) as usize;
    for (vk, held) in HELD.iter().enumerate() {
        let down = vk == trigger && unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 };
        held.store(down, Ordering::Relaxed);
    }
}

/// Start counting, or join the count already running. Blocks for the millisecond or two it takes
/// the hooks to go in, so nothing after the keypress is missed.
fn watch() -> Option<Arc<Watch>> {
    let mut current = WATCH.lock();
    if let Some(w) = current.upgrade() {
        return Some(w);
    }
    // Without both counts the gate cannot tell that nothing moved: no watch at all.
    let hooks = win_hook::start(
        vec![(WH_KEYBOARD_LL, Some(on_key)), (WH_MOUSE_LL, Some(on_mouse))],
        Duration::from_millis(500),
        mark_held_keys,
    )?;
    let w = Arc::new(Watch { hooks });
    *current = Arc::downgrade(&w);
    Some(w)
}

// ---------------------------------------------------------------------------- the stamp

/// The foreground window, and the control in it that had keyboard focus.
fn front() -> Option<(isize, isize, u32)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        let thread = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let focus = if GetGUIThreadInfo(thread, &mut info).is_ok() { info.hwndFocus.0 as isize } else { 0 };
        Some((hwnd.0 as isize, focus, pid))
    }
}

pub fn class_of(hwnd: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Windows that are not somewhere to type: the desktop and the taskbar.
fn is_shell(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// What was in front and how much he had touched, at the moment the shortcut arrived.
#[derive(Debug, Clone)]
pub struct FocusStamp {
    window: isize,
    focus: isize,
    pid: u32,
    class: String,
    clicks: u32,
    keys: u32,
    /// Another key was already held when the dictation started.
    held_other: bool,
    /// Held, never read: while any stamp of this dictation lives, the count keeps running.
    _watch: Arc<Watch>,
}

impl FocusStamp {
    /// Pure user32 calls - microseconds - plus starting the input count.
    pub fn capture() -> Option<Self> {
        let (window, focus, pid) = front()?;
        let class = class_of(window);
        if pid == std::process::id() || is_shell(&class) {
            return None;
        }
        let watch = watch()?;
        Some(FocusStamp {
            window,
            focus,
            pid,
            class,
            clicks: CLICKS.load(Ordering::Relaxed),
            keys: KEYS.load(Ordering::Relaxed),
            // Read with the hooks in, so a key pressed since is counted by them instead.
            held_other: other_key_held(),
            _watch: watch,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Chrome, Edge and every Electron app draw into one window of this class. Their text
    /// fields are not native controls, and asking them for their accessibility tree switches
    /// some of them (VS Code) into screen-reader mode - so they are recognised here instead.
    pub fn is_chromium_window(&self) -> bool {
        self.class.starts_with("Chrome_WidgetWin")
    }

    fn window_moved(&self) -> Option<&'static str> {
        match front() {
            None => Some("no-front-window"),
            Some((window, _, _)) if window != self.window => Some("window-changed"),
            Some((_, focus, _)) if focus != self.focus => Some("focus-changed"),
            Some(_) => None,
        }
    }

    fn counted(&self) -> (u32, u32) {
        (
            CLICKS.load(Ordering::Relaxed).wrapping_sub(self.clicks),
            KEYS.load(Ordering::Relaxed).wrapping_sub(self.keys),
        )
    }

    /// Anything at all since the keypress - for a capture made a moment after it, before even
    /// the stop press.
    pub fn moved_since_keypress(&self) -> Option<&'static str> {
        if self.held_other {
            return Some("key-held-at-start");
        }
        self.window_moved().or(match self.counted() {
            (0, 0) => None,
            (0, _) => Some("typed"),
            _ => Some("clicked"),
        })
    }

    /// Why a paste would no longer land in the control that was focused at the keypress: the
    /// window or focused control changed, he clicked, or he pressed any key besides the stop.
    pub fn moved(&self) -> Option<&'static str> {
        if self.held_other {
            return Some("key-held-at-start");
        }
        let (clicks, keys) = self.counted();
        let expected = super::box_input::expected(false);
        self.window_moved().or_else(|| input_moved(clicks, keys, expected))
    }
}

// ---------------------------------------------------------------------------- the keystroke

fn held(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
/// Every modifier other than Ctrl, left and right apart, so each is put back exactly.
const OTHER_MODIFIERS: [u16; 6] = [0xA0, 0xA1, 0xA4, 0xA5, VK_LWIN.0, VK_RWIN.0]; // Shift, Alt, Win
/// An unassigned key. Pressed between Alt (or Win) going down and coming up, it stops Windows
/// reading "Alt pressed on its own" as "open the menu bar" (or Win as "open Start") - the same
/// "menu mask" trick AutoHotkey uses, and for the same reason.
const VK_MASK: u16 = 0xE8;

fn key(vk: u16, up: bool) -> INPUT {
    // Right-hand Ctrl and Alt and both Win keys are "extended" keys; without the flag Windows
    // reads them as their left-hand twins.
    let extended = matches!(vk, 0xA3 | 0xA5 | 0x5B | 0x5C);
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    inputs.is_empty()
        || unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } as usize == inputs.len()
}

/// The keystrokes for a Ctrl+V that lands as Ctrl+V whatever he is still holding.
///
/// A synthetic Ctrl+V sent while Alt or Shift is down arrives as Ctrl+Alt+V or Ctrl+Shift+V,
/// which pastes nothing. macOS avoids this with a private event source; Windows has none, so the
/// other modifiers are let go of for the paste and pressed again straight after - one `SendInput`,
/// which Windows delivers without any real key press in between. His fingers never notice.
fn paste_keys(ctrl_held: bool, others_held: &[u16]) -> Vec<INPUT> {
    let alt_or_win = others_held.iter().any(|k| matches!(*k, 0xA4 | 0xA5 | 0x5B | 0x5C));
    let mut keys = Vec::new();
    if alt_or_win {
        keys.extend([key(VK_MASK, false), key(VK_MASK, true)]);
    }
    keys.extend(others_held.iter().map(|k| key(*k, true)));
    if !ctrl_held {
        keys.push(key(VK_LCONTROL, false));
    }
    keys.extend([key(VK_V, false), key(VK_V, true)]);
    if !ctrl_held {
        keys.push(key(VK_LCONTROL, true));
    }
    keys.extend(others_held.iter().map(|k| key(*k, false)));
    if alt_or_win {
        // So that letting go of them afterwards opens no menu either.
        keys.extend([key(VK_MASK, false), key(VK_MASK, true)]);
    }
    keys
}

/// Press Ctrl+V in whatever has keyboard focus, at once, even with the shortcut still held.
/// Every caller decides first that focus is right.
pub fn press_paste() -> Result<(), DeliveryError> {
    let ctrl_held = held(VK_LCONTROL) || held(VK_RCONTROL);
    let others: Vec<u16> = OTHER_MODIFIERS.iter().copied().filter(|k| held(*k)).collect();
    if send(&paste_keys(ctrl_held, &others)) {
        Ok(())
    } else {
        Err(DeliveryError::Other("Windows refused the paste keystroke".into()))
    }
}

/// Called the instant a shortcut with Alt or Win in it fires. Windows swallows the shortcut's
/// own key, so the app underneath sees Alt go down and come up with nothing in between - and
/// opens its menu bar (or, for Win, the Start menu). The mask key in between prevents that.
pub fn mask_menu_key() {
    if [0xA4, 0xA5, VK_LWIN.0, VK_RWIN.0].iter().any(|k| held(*k)) {
        send(&[key(VK_MASK, false), key(VK_MASK, true)]);
    }
}

/// Paste `text` from the normal clipboard, then put back whatever was there. The app reads the
/// clipboard when it handles the keystroke, a moment later, so the give-back waits for it.
///
/// If his clipboard cannot be remembered in full, it is not borrowed and nothing is pasted: the
/// words stay on Huck's clipboard, and what he copied stays exactly where it was.
pub fn paste_borrowing_clipboard(text: &str) -> Result<(), DeliveryError> {
    let borrowed = crate::clip::huck::borrow_general(text)
        .map_err(|why| DeliveryError::Other(format!("his clipboard was left alone: {why}")))?;
    let pressed = press_paste();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let _ = borrowed.give_back();
    });
    pressed
}

// ---------------------------------------------------------------------------- the destination

/// Paste into the control that was focused at the keypress, if - and only if - it still is.
pub struct PasteDestination {
    stamp: FocusStamp,
    label: String,
    /// Huck's own clipboard is chosen, so the normal one does not hold the words yet.
    borrow: bool,
    /// What a click or key press inside the same window may be forgiven by.
    rule: SameWindow,
}

/// After a click or key press inside the same window and focused control, may the paste still go?
pub enum SameWindow {
    /// No: nothing can tell which box the caret is in now. The words wait on the clipboard.
    Strict,
    /// Yes, where the caret is: Chrome and Electron apps (VS Code, Slack) draw every box into one
    /// control and name none of them - the Mac's rule for VS Code. In Chrome that includes the
    /// address bar (decided with him 2026-09-29): typed there, never sent, still on the clipboard.
    Caret,
    /// Only if UI Automation's focused element is still the box from the keypress.
    SameBox(super::windows_uia::SameElement),
}

/// Where the caret is after he clicked or typed inside the same window and control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Unknown,
    Caret,
    SameBox,
    AnotherBox,
}

impl PasteDestination {
    pub fn new(stamp: FocusStamp, label: String, borrow: bool) -> Self {
        PasteDestination { stamp, label, borrow, rule: SameWindow::Strict }
    }

    /// Forgive clicks and typing inside the same window by this rule (see `gone`).
    pub fn forgiving(mut self, rule: SameWindow) -> Self {
        self.rule = rule;
        self
    }

    fn place(&self) -> Place {
        match &self.rule {
            SameWindow::Strict => Place::Unknown,
            SameWindow::Caret => Place::Caret,
            SameWindow::SameBox(field) if field.is_focused() => Place::SameBox,
            SameWindow::SameBox(_) => Place::AnotherBox,
        }
    }

    /// Why a paste would not land in his box, or `None` when it would.
    ///
    /// Clicks or typing since the keypress may have moved the caret to another box. Decided with
    /// him 2026-09-29, the twin of `macos_paste`'s rule: he clicks away and back into his box
    /// before sending, and expects it to arrive. Within the same window and focused control, that
    /// is forgiven only where something can say where the caret went (`SameWindow`). Another
    /// window or control, an unreadable focus, or the stop press not seen, still refuses.
    fn gone(&self) -> Option<&'static str> {
        if let Some(why) = self.stamp.window_moved() {
            return Some(why);
        }
        let (clicks, keys) = self.stamp.counted();
        let expected = super::box_input::expected(false);
        let strict =
            if self.stamp.held_other { Some("key-held-at-start") } else { input_moved(clicks, keys, expected) };
        let verdict = after_moving(strict, keys >= expected.1, self.stamp.focus != 0, || self.place());
        let (Some(why), None) = (strict, verdict) else { return verdict };
        eprintln!("[hvtt] paste gate: {why}, same window and box - forgiven if nothing moves now");
        // Asking UI Automation (and the log line) take a moment, in which he could have clicked or
        // switched away. Everything read before is read again, last, and must not have changed.
        // (Codex's third and fourth reviews, 2026-09-29.)
        settled((clicks, keys), self.stamp.counted(), self.stamp.window_moved())
    }
}

/// Nothing moved while the same-window check ran: the same window and control, and no click or
/// key press since the counts were first read.
fn settled(before: (u32, u32), after: (u32, u32), window_moved: Option<&'static str>) -> Option<&'static str> {
    window_moved.or((before != after).then_some("moved-during-check"))
}

/// The same-window rule, apart from the system calls. `strict` is the old verdict (anything at
/// all refuses); `stop_seen` is the key count reaching the stop press, which proves the count ran;
/// `place` is asked only when it matters. (Codex's review, 2026-09-29: a click used to hide a
/// missing stop press, and a same `hwndFocus` was taken to mean the same box.)
///
/// The forgiving places do not rest on the counts: `Caret` pastes where the caret is whatever he
/// did, and `SameBox` asks UI Automation directly. The counts decide only `Strict`, as before, and
/// `stop_seen` is a check that they were running at all - not proof of which key was pressed.
fn after_moving(
    strict: Option<&'static str>,
    stop_seen: bool,
    focus_known: bool,
    place: impl FnOnce() -> Place,
) -> Option<&'static str> {
    let why = strict?;
    if !stop_seen {
        return Some("stop-not-seen");
    }
    if !focus_known {
        return Some(why);
    }
    match place() {
        Place::Caret | Place::SameBox => None,
        Place::AnotherBox => Some("another-box"),
        Place::Unknown => Some(why),
    }
}

impl Destination for PasteDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        match self.gone() {
            None => Liveness::Alive,
            Some(why) => Liveness::dead(why),
        }
    }

    fn deliver(&self, text: &str, copied: bool) -> Result<(), DeliveryError> {
        // The paste keys send what the clipboard holds. With the normal clipboard chosen that
        // is the copy the pipeline made just before - and if that copy failed, it is whatever he
        // had copied earlier. Refused here, in the one place every paste goes through, whoever
        // calls it (a direct-write destination falling back to it included).
        if !super::words_are_there_to_paste(self.borrow, copied) {
            return Err(DeliveryError::Other("the clipboard did not take the words".into()));
        }
        // Transcription took time in which he could have clicked away: refuse before touching his
        // clipboard at all.
        if self.gone().is_some() {
            return Err(DeliveryError::DestinationLost);
        }
        // Anything that can wait - borrowing the clipboard waits on whichever program holds it -
        // happens before the last check, so nothing stands between that check and the keystroke.
        // (Codex's fourth review, 2026-09-29.)
        let borrowed = match self.borrow {
            true => Some(
                crate::clip::huck::borrow_general(text)
                    .map_err(|why| DeliveryError::Other(format!("his clipboard was left alone: {why}")))?,
            ),
            false => None,
        };
        let pressed = match self.gone() {
            Some(_) => Err(DeliveryError::DestinationLost),
            None => press_paste(),
        };
        if let Some(borrowed) = borrowed {
            // The app reads the clipboard when it handles the keystroke, a moment later.
            let wait = if pressed.is_ok() { Duration::from_millis(500) } else { Duration::ZERO };
            std::thread::spawn(move || {
                std::thread::sleep(wait);
                let _ = borrowed.give_back();
            });
        }
        pressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_key_besides_the_stop_press_refuses_the_paste() {
        // Codex's blocker: Arrow Left then the stop shortcut used to pass (three were allowed).
        for vk in [0x25, 0x26, 0x27, 0x28, 0x41, 0x31, 0x08, 0x0D, 0x2E, 0x20] {
            assert!(counts_as_typing(vk, false, false), "{vk:#x} can move the caret");
        }
        let plain = (0, 1);
        assert_eq!(input_moved(0, 1, plain), None, "the stop press alone");
        assert_eq!(input_moved(0, 2, plain), Some("typed"), "an arrow or a letter, then the stop press");
        assert_eq!(input_moved(0, 3, plain), Some("typed"));
        assert_eq!(input_moved(1, 1, plain), Some("clicked"));
        assert_eq!(input_moved(0, 0, plain), Some("stop-not-seen"), "no count at all is not proof");
    }

    #[test]
    fn work_in_the_box_is_allowed_and_nothing_more() {
        // Paused, fixed a word (two clicks, six keys), then pressed the shortcut.
        let edited = (2, 7);
        assert_eq!(input_moved(2, 7, edited), None);
        assert_eq!(input_moved(3, 7, edited), Some("clicked"), "a click outside the box");
        assert_eq!(input_moved(2, 8, edited), Some("typed"), "a key outside the box");
        assert_eq!(input_moved(1, 7, edited), Some("clicked"), "counts that do not add up refuse");
        // Sent with the button: no stop press is expected.
        assert_eq!(input_moved(1, 0, (1, 0)), None);
    }

    #[test]
    fn clicking_away_and_back_in_the_same_window_still_sends() {
        // Decided with him 2026-09-29: the Mac's rule, where something can say where the caret is.
        let never = || -> Place { panic!("not asked when nothing moved") };
        assert_eq!(after_moving(None, true, true, never), None, "nothing moved");
        assert_eq!(after_moving(Some("clicked"), true, true, || Place::Caret), None, "VS Code");
        assert_eq!(after_moving(Some("typed"), true, true, || Place::SameBox), None, "back in his box");
        assert_eq!(after_moving(Some("key-held-at-start"), true, true, || Place::SameBox), None);
        assert_eq!(after_moving(Some("clicked"), true, true, || Place::AnotherBox), Some("another-box"));
        assert_eq!(after_moving(Some("clicked"), true, true, || Place::Unknown), Some("clicked"), "no rule: strict");
    }

    #[test]
    fn a_click_never_hides_a_missing_stop_press() {
        // Codex's review, 2026-09-29.
        for why in ["clicked", "typed", "key-held-at-start"] {
            assert_eq!(after_moving(Some(why), false, true, || Place::Caret), Some("stop-not-seen"), "{why}");
        }
        assert_eq!(after_moving(Some("stop-not-seen"), false, true, || Place::Caret), Some("stop-not-seen"));
    }

    #[test]
    fn nothing_may_move_while_the_same_window_check_runs() {
        // Codex's third review, 2026-09-29: focus could change during the UI Automation check.
        assert_eq!(settled((2, 3), (2, 3), None), None);
        assert_eq!(settled((2, 3), (3, 3), None), Some("moved-during-check"), "a click meanwhile");
        assert_eq!(settled((2, 3), (2, 4), None), Some("moved-during-check"), "a key meanwhile");
        assert_eq!(settled((2, 3), (2, 3), Some("focus-changed")), Some("focus-changed"));
        assert_eq!(settled((2, 3), (2, 3), Some("window-changed")), Some("window-changed"));
    }

    #[test]
    fn an_unreadable_focus_forgives_nothing() {
        assert_eq!(after_moving(Some("clicked"), true, false, || Place::Caret), Some("clicked"));
        assert_eq!(after_moving(Some("typed"), true, false, || Place::SameBox), Some("typed"));
    }

    #[test]
    fn the_trigger_key_is_found_from_the_shortcut() {
        set_trigger_key("Alt+Space");
        assert_eq!(TRIGGER.load(Ordering::Relaxed), 0x20);
        set_trigger_key("Ctrl+Alt+Shift+V");
        assert_eq!(TRIGGER.load(Ordering::Relaxed), 0x56);
        set_trigger_key("Ctrl+KeyD");
        assert_eq!(TRIGGER.load(Ordering::Relaxed), 0x44);
        set_trigger_key("Super+F5");
        assert_eq!(TRIGGER.load(Ordering::Relaxed), 0x74);
        set_trigger_key("Alt+Space");
    }

    #[test]
    fn mouse_buttons_are_not_keys_held_at_the_start() {
        for vk in [0x01, 0x02, 0x04, 0x05, 0x06] {
            assert!(is_mouse_button(vk));
        }
        assert!(!is_mouse_button(0x25), "Left Arrow is a key");
    }

    #[test]
    fn a_held_shortcut_types_nothing_and_its_modifiers_are_known() {
        // Alt+Space held: Space's presses are kept from every program...
        let space = swallowing(7, 0x20);
        assert!(swallowed(0x20, true, false, space));
        // ...but not its release, another key, our own keystrokes, or any key once it is up.
        assert!(!swallowed(0x20, false, false, space), "the release goes through");
        assert!(!swallowed(0x41, true, false, space), "another key");
        assert!(!swallowed(0x20, true, true, space), "a keystroke of our own");
        // The same key held for two holds is two different entries: tidying one leaves the other.
        assert_ne!(swallowing(7, 0x20), swallowing(8, 0x20));
        assert!(!swallowed(0x20, true, false, 0), "not held any more");
        assert!(!swallowed(0, true, false, 0), "no shortcut key known");

        assert_eq!(modifiers_of("Alt+Space"), 2);
        assert_eq!(modifiers_of("Ctrl+Alt+Shift+V"), 1 | 2 | 4);
        assert_eq!(modifiers_of("F9"), 0);
        // Either Alt is the shortcut's Alt; Space is no modifier.
        assert_eq!(modifier_bit(0xA4), 2);
        assert_eq!(modifier_bit(0xA5), 2);
        assert_eq!(modifier_bit(0x20), 0);
    }

    #[test]
    fn repeats_and_our_own_keystrokes_are_not_typing() {
        assert!(!counts_as_typing(0x20, false, true), "the shortcut's auto-repeat");
        assert!(!counts_as_typing(0x56, true, false), "our own Ctrl+V");
        assert!(!counts_as_typing(0xE8, true, false), "our menu mask");
    }

    #[test]
    fn modifiers_are_not_counted_as_typing() {
        for vk in [0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C] {
            assert!(is_modifier(vk), "{vk:#x} is a modifier");
        }
        for vk in [0x20, 0x41, 0x56, 0x0D] {
            assert!(!is_modifier(vk), "{vk:#x} is a real key press");
        }
    }

    fn spelled(inputs: &[INPUT]) -> Vec<(u16, bool)> {
        inputs
            .iter()
            .map(|i| unsafe { (i.Anonymous.ki.wVk.0, i.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP)) })
            .collect()
    }

    #[test]
    fn a_paste_with_nothing_held_is_plain_ctrl_v() {
        assert_eq!(spelled(&paste_keys(false, &[])), vec![(0xA2, false), (0x56, false), (0x56, true), (0xA2, true)]);
    }

    #[test]
    fn a_paste_under_held_ctrl_alt_is_still_ctrl_v_and_gives_the_keys_back() {
        // Ctrl + Alt still down from the shortcut: Alt is let go for the V and pressed again,
        // with the mask key either side so neither release opens the menu bar. Ctrl is his.
        let keys = spelled(&paste_keys(true, &[0xA4]));
        assert_eq!(
            keys,
            vec![
                (VK_MASK, false), (VK_MASK, true),
                (0xA4, true),
                (0x56, false), (0x56, true),
                (0xA4, false),
                (VK_MASK, false), (VK_MASK, true),
            ]
        );
    }

    #[test]
    fn shift_alone_needs_no_mask() {
        let keys = spelled(&paste_keys(true, &[0xA0]));
        assert!(!keys.iter().any(|(k, _)| *k == VK_MASK));
        assert_eq!(keys.first(), Some(&(0xA0, true)));
        assert_eq!(keys.last(), Some(&(0xA0, false)));
    }

    #[test]
    fn the_desktop_and_taskbar_are_never_a_destination() {
        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            assert!(is_shell(class));
        }
        assert!(!is_shell("Notepad"));
        assert!(!is_shell("Chrome_WidgetWin_1"));
    }
}
