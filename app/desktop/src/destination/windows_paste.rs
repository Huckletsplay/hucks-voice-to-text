//! The paste rung on Windows: the twin of `macos_paste`, for apps with no silent write path -
//! Electron (VS Code, Slack, Discord, the Claude and ChatGPT apps), Chrome without the extension,
//! Windows Terminal, Word.
//!
//! A paste lands in *whatever is focused*, so it is gated on the one thing that makes it safe:
//! **nothing has moved since the keypress**. Same foreground window, same focused control, no
//! mouse click, no typing beyond the shortcut itself. If any of that changed it refuses, and the
//! words wait on the clipboard.
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

/// Tell the gate which key starts and stops dictation, from its accelerator (`Alt+Space`).
pub fn set_trigger_key(accelerator: &str) {
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

/// The input half of the gate, from what the hooks counted since the keypress.
fn input_moved(clicks: u32, keys: u32) -> Option<&'static str> {
    if clicks != 0 {
        return Some("clicked");
    }
    match keys {
        1 => None,
        0 => Some("stop-not-seen"),
        _ => Some("typed"),
    }
}

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let vk = (info.vkCode & 0xFF) as usize;
        let message = wparam.0 as u32;
        let injected = info.flags.contains(LLKHF_INJECTED);
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
        self.window_moved().or_else(|| input_moved(clicks, keys))
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
}

impl PasteDestination {
    pub fn new(stamp: FocusStamp, label: String, borrow: bool) -> Self {
        PasteDestination { stamp, label, borrow }
    }
}

impl Destination for PasteDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        match self.stamp.moved() {
            None => Liveness::Alive,
            Some(why) => Liveness::dead(why),
        }
    }

    fn deliver(&self, text: &str) -> Result<(), DeliveryError> {
        // Checked again last thing: transcription took time in which he could have clicked away.
        if self.stamp.moved().is_some() {
            return Err(DeliveryError::DestinationLost);
        }
        if self.borrow {
            paste_borrowing_clipboard(text)
        } else {
            press_paste()
        }
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
        assert_eq!(input_moved(0, 1), None, "the stop press alone");
        assert_eq!(input_moved(0, 2), Some("typed"), "an arrow or a letter, then the stop press");
        assert_eq!(input_moved(0, 3), Some("typed"));
        assert_eq!(input_moved(1, 1), Some("clicked"));
        assert_eq!(input_moved(0, 0), Some("stop-not-seen"), "no count at all is not proof");
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
