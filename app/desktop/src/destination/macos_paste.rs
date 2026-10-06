//! The paste rung: for apps with no silent write path - Electron (VS Code, Slack, Discord, the
//! Claude and ChatGPT apps) and Chrome without the extension.
//!
//! A paste lands in *whatever is focused*, which is why there was no paste path at all: when the
//! field-capture spike's target died, its paste put the words in an unrelated application. So
//! this rung is gated on the one thing that makes a paste safe - **the same frontmost app and the
//! same front window as at the keypress, with that app still holding the keyboard**. Another app
//! or window refuses, and so does a launcher's panel that took the keyboard (the window list
//! alone misses that: see `active_app`); the words wait on the clipboard. So does a stop press
//! that was never counted, which would mean the count was not running.
//!
//! Clicks and key presses inside that window are counted too. Since 2026-09-29 they are forgiven
//! where something can say where the caret went (`SameWindow`), because he clicks away and back
//! into his box before sending: always in Chrome and Electron apps, and elsewhere only when
//! Accessibility sees the very box from the keypress. The twin of `windows_paste`, which got
//! Codex's four review findings first; they apply here in the same way (see `after_moving`,
//! `settled` and `deliver`).
//!
//! Electron apps do not expose their text fields to Accessibility unless a screen-reader mode is
//! forced on, which changes how those apps behave, so the gate works at window and input level
//! rather than on the field itself.
//!
//! Cmd+V only ever reads the normal clipboard. On the normal-clipboard setting it already holds
//! the transcript (`complete_transcription` copies before it delivers). On Huck's own clipboard
//! the words are put on the normal one for half a second and whatever he had there is put back.

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use hvtt_core::pipeline::{DeliveryError, Destination, Liveness};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

const HID_SYSTEM_STATE: i32 = 1; // kCGEventSourceStateHIDSystemState
const PRIVATE_STATE: i32 = -1; // kCGEventSourceStatePrivate
const LEFT_MOUSE_DOWN: u32 = 1;
const RIGHT_MOUSE_DOWN: u32 = 3;
const KEY_DOWN: u32 = 10;
const OTHER_MOUSE_DOWN: u32 = 25;
const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;
const HID_EVENT_TAP: u32 = 0; // kCGHIDEventTap
const KEY_V: u16 = 9;
const FLAG_COMMAND: u64 = 0x0010_0000;

// Exactly one key press is expected between capture and delivery beyond what he typed inside
// Huck's box: the stop shortcut's ordinary key (`box_input::expected`). Core Graphics' cumulative
// counter includes auto-repeat and cannot identify which key repeated, so a held stop shortcut
// counts as extra presses - which the same-window rule may forgive, but only once the stop press
// itself has been seen. The words remain on the chosen clipboard in every refusal.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    fn CGEventSourceCounterForEventType(state: i32, event_type: u32) -> u32;
    fn CGEventSourceCreate(state: i32) -> CFTypeRef;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, key: u16, down: bool) -> CFTypeRef;
    fn CGEventSetFlags(event: CFTypeRef, flags: u64);
    fn CGEventPost(tap: u32, event: CFTypeRef);
    static kCGWindowLayer: CFStringRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowNumber: CFStringRef;
}

/// Mouse presses macOS delivered to this app's own windows - the box, the H. Counted natively
/// because the page cannot see them all: on 2026-09-28 the system saw 6 clicks during a dictation
/// and the page 5, and a paste into VS Code was refused although he never left the box.
static OWN_CLICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Start counting clicks on this app's windows. Main thread, once, at launch. If it cannot start,
/// clicks in the box look like clicks elsewhere: a paste is refused, never misdirected.
pub fn count_own_clicks() {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ptr::NonNull;
    // NSEventMaskLeftMouseDown | RightMouseDown | OtherMouseDown
    const MASK: u64 = (1 << 1) | (1 << 3) | (1 << 25);
    let Some(class) = AnyClass::get(c"NSEvent") else { return };
    let handler = block2::RcBlock::new(|event: NonNull<AnyObject>| -> *mut AnyObject {
        OWN_CLICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        event.as_ptr()
    });
    unsafe {
        let monitor: *mut AnyObject =
            msg_send![class, addLocalMonitorForEventsMatchingMask: MASK, handler: &*handler];
        // Kept for the life of the app.
        std::mem::forget(Retained::retain(monitor));
    }
}

fn own_clicks() -> u32 {
    OWN_CLICKS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The system's running counters at one moment - or what they gained between two moments. They
/// wrap around, so a difference is always taken with `since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    /// Mouse presses anywhere.
    clicks: u32,
    /// Those made on this app's own windows - the box, the H.
    own_clicks: u32,
    /// Key presses, auto-repeat included.
    keys: u32,
}

impl Counts {
    fn now() -> Self {
        Counts { clicks: clicks(), own_clicks: own_clicks(), keys: keys() }
    }

    /// What was counted between `before` and this moment.
    fn since(self, before: Counts) -> Counts {
        Counts {
            clicks: self.clicks.wrapping_sub(before.clicks),
            own_clicks: self.own_clicks.wrapping_sub(before.own_clicks),
            keys: self.keys.wrapping_sub(before.keys),
        }
    }
}

/// What was in front, and how much he had touched, at the moment the shortcut arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusStamp {
    pid: i32,
    window: i64,
    at_keypress: Counts,
}

impl FocusStamp {
    /// Microseconds for the counters; under a millisecond for the window list once warm.
    pub fn capture() -> Option<Self> {
        let (pid, window) = front_window()?;
        Some(FocusStamp { pid, window, at_keypress: Counts::now() })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Whether the same app's same window is still in front.
    fn window_moved(&self) -> Option<&'static str> {
        match front_window() {
            None => Some("no-front-window"),
            Some((pid, _)) if pid != self.pid => Some("app-switched"),
            Some((_, window)) if window != self.window => Some("window-changed"),
            Some(_) => None,
        }
    }

    /// The fast half of "where is he" - microseconds, so it can be the last thing read before the
    /// keystroke: the same window in front, and the same app active.
    fn fast_moved(&self) -> Option<&'static str> {
        self.window_moved().or_else(|| active_verdict(self.pid, active_app()))
    }

    /// The slow half: whether Accessibility names another app as holding the keyboard. As long as
    /// the other app takes to answer, so it is asked *before* the last re-read, never after it.
    fn named_elsewhere(&self) -> Option<&'static str> {
        named_verdict(self.pid, super::macos_ax::focused_app_pid())
    }

    /// What he clicked and typed since the keypress.
    fn counted(&self) -> Counts {
        Counts::now().since(self.at_keypress)
    }
}

/// The keyboard half of "where is he": the active app must be the one stamped at the keypress, and
/// Accessibility must not name another. The window list alone is not enough - a launcher's panel
/// floats above his window, which stays "in front" behind it, while the keyboard goes to the panel
/// (Codex's reviews of 0.1.6). Nothing is forgiven here: not a click, not typing. Huck's own box
/// holding the keyboard refuses too; the keyboard is handed back, and waited for, before anything
/// is delivered (`hand_back_keyboard`).
///
/// Two independent signals, because each misses a case (both measured, 2026-09-29):
/// - `active` (`active_app`) sees a panel that *activates* its app, and names none for an
///   unreadable moment, which refuses;
/// - `named` (Accessibility's focused application) sees a panel that takes the keyboard *without*
///   activating its app - and says nothing (`None`) with an Electron app in front, which is not a
///   reason to refuse. It is an alarm only: a named app that is not the stamped one.
///
/// Still not seen: such a non-activating panel belonging to an app that does not answer
/// Accessibility either.
fn keyboard_verdict(stamped: i32, active: Option<i32>, named: Option<i32>) -> Option<&'static str> {
    active_verdict(stamped, active).or_else(|| named_verdict(stamped, named))
}

/// The active app must be the stamped one; none named refuses.
fn active_verdict(stamped: i32, active: Option<i32>) -> Option<&'static str> {
    match active {
        Some(pid) if pid == stamped => None,
        Some(_) => Some("keyboard-elsewhere"),
        None => Some("no-active-app"),
    }
}

/// Accessibility naming another app refuses; Accessibility naming no one decides nothing.
fn named_verdict(stamped: i32, named: Option<i32>) -> Option<&'static str> {
    match named {
        Some(pid) if pid != stamped => Some("keyboard-elsewhere"),
        _ => None,
    }
}

/// Pay the window server's one-off setup cost (~45 ms) at launch, not on the first keypress.
pub fn warm_up() {
    let _ = front_window();
}

/// The frontmost ordinary window: its owner and its window number. Our own floating box and
/// every other panel sit above layer 0, so they are never mistaken for the target.
fn front_window() -> Option<(i32, i64)> {
    let own = std::process::id() as i64;
    unsafe {
        let raw = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if raw.is_null() {
            return None;
        }
        let list: CFArray<CFDictionary<CFString, CFType>> = CFArray::wrap_under_create_rule(raw);
        let (layer_key, pid_key, number_key) = (
            CFString::wrap_under_get_rule(kCGWindowLayer),
            CFString::wrap_under_get_rule(kCGWindowOwnerPID),
            CFString::wrap_under_get_rule(kCGWindowNumber),
        );
        let number = |d: &CFDictionary<CFString, CFType>, k: &CFString| {
            d.find(k).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64())
        };
        list.iter().find_map(|d| {
            let pid = number(&d, &pid_key)?;
            (number(&d, &layer_key)? == 0 && pid != own)
                .then(|| Some((pid as i32, number(&d, &number_key)?)))
                .flatten()
        })
    }
}

fn clicks() -> u32 {
    unsafe {
        [LEFT_MOUSE_DOWN, RIGHT_MOUSE_DOWN, OTHER_MOUSE_DOWN]
            .iter()
            .map(|t| CGEventSourceCounterForEventType(HID_SYSTEM_STATE, *t))
            .fold(0u32, u32::wrapping_add)
    }
}

/// The system's own count of key presses, and nothing taken from it. A held dictation's key
/// repeating is **not** left out: the presses the tap keeps back were subtracted here for an
/// afternoon, and Codex's review (2026-10-05) showed that a repeat arriving between the two reads
/// cancelled a real key press - a Tab into another box, then a paste there. If the system counts
/// those repeats, a held dictation is "typed-or-repeated", which the same-window rule forgives
/// where it can say where the caret is and otherwise only copies. Safe either way.
fn keys() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID_SYSTEM_STATE, KEY_DOWN) }
}

// ---------------------------------------------------------------------------- hold to talk
//
// Dictation Style > Hold to Talk: he holds the shortcut, talks, and lets go. The twin of
// `windows_paste`'s section of the same name, for the same two reasons, and a third of the Mac's
// own.
//
// 1. Its own key must not type. macOS keeps repeating the last key pressed. While every key of
//    Option+Space is down those repeats are the shortcut again and reach nobody; the moment he
//    lets go of Option first, they are plain Spaces, typed into his text box for as long as his
//    thumb is still down. So from the start of a held dictation until that key comes up, its
//    presses are kept back by an event tap. (Only presses: the release goes through, so the
//    shortcut's own watcher still sees the key come up.)
// 2. Letting go of ANY of its keys is letting go. The shortcut library reports a release only
//    for the main key; Option up, Space still down, would otherwise keep recording.
//
// The gate's key count is left alone (`keys`): the presses kept back here are counted only to be
// said in the log.
//
// The tap needs Accessibility, which delivery needs anyway. Without it a key let go is still
// noticed, by looking; only the keeping-back is lost.

const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
const SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
const FIELD_AUTOREPEAT: u32 = 8; // kCGKeyboardEventAutorepeat
const FIELD_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode
const FIELD_SOURCE_STATE: u32 = 45; // kCGEventSourceStateID
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
/// No key: the Mac's key numbers start at 0, which is A.
const NO_KEY: u32 = u32::MAX;

/// The key number of the dictation shortcut's own key (49, Space, for Option+Space).
static TRIGGER: AtomicU32 = AtomicU32::new(NO_KEY);
/// The dictation shortcut's modifiers, as event flags.
static TRIGGER_MODIFIERS: AtomicU64 = AtomicU64::new(0);

/// The held dictation whose keys are being watched, by a number of its own; 0 for none. One of
/// its shortcut's keys coming up ends it - once.
static HOLDING: AtomicU64 = AtomicU64::new(0);
static HOLD_SERIAL: AtomicU64 = AtomicU64::new(0);
/// That hold's own keys, as they were when it began - not the shortcut as it is now, which he
/// can change while a key is still down.
static HOLD_KEY: AtomicU32 = AtomicU32::new(NO_KEY);
static HOLD_MODIFIERS: AtomicU64 = AtomicU64::new(0);
/// The key whose presses reach no program - a held dictation's own, until it comes up - with
/// the number of the hold it is for (`swallowing`); 0 for none.
static SWALLOWING: AtomicU64 = AtomicU64::new(0);
/// Every press kept back so far, since the program started. For the log only.
static KEPT_BACK: AtomicU32 = AtomicU32::new(0);
/// The tap, once made, and the hold it is switched on for; 0 when it is off.
static TAP: Mutex<Option<Tap>> = Mutex::new(None);
static TAP_FOR: Mutex<u64> = Mutex::new(0);
/// The same, for the tap's own thread to read without waiting on anything.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static TAP_WANTED: AtomicBool = AtomicBool::new(false);
/// How the program is told that a key of a held shortcut came up, and of which hold
/// (`on_hold_key_up`).
static HOLD_KEY_UP: std::sync::OnceLock<Box<dyn Fn(u64) + Send + Sync>> = std::sync::OnceLock::new();

#[derive(Clone, Copy)]
struct Tap(CFTypeRef);
// A Mach port, used only through Core Graphics' own thread-safe calls.
unsafe impl Send for Tap {}

type TapCallback = extern "C" fn(*mut c_void, u32, CFTypeRef, *mut c_void) -> CFTypeRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: TapCallback,
        user: *mut c_void,
    ) -> CFTypeRef;
    fn CGEventTapEnable(tap: CFTypeRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CFTypeRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CFTypeRef) -> u64;
}

/// Set once, when the program starts. `told` is given the hold's number (`hold_started`).
pub fn on_hold_key_up(told: impl Fn(u64) + Send + Sync + 'static) {
    let _ = HOLD_KEY_UP.set(Box::new(told));
}

/// A swallowed key and the hold it belongs to, as one number.
fn swallowing(serial: u64, key: u32) -> u64 {
    (serial << 16) | (key as u64 & 0xFFFF)
}

fn key_down(key: u32) -> bool {
    key != NO_KEY && unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, key as u16) }
}

/// Is every modifier in `wanted` held - on either side of the keyboard?
fn modifiers_down(wanted: u64) -> bool {
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) & wanted == wanted }
}

/// The key number of a shortcut's own key, from its name in an accelerator (`Space`, `V`, `F5`).
fn key_number(name: &str) -> Option<u32> {
    const KEYS: &[(&str, u32)] = &[
        ("A", 0), ("S", 1), ("D", 2), ("F", 3), ("H", 4), ("G", 5), ("Z", 6), ("X", 7), ("C", 8),
        ("V", 9), ("B", 11), ("Q", 12), ("W", 13), ("E", 14), ("R", 15), ("Y", 16), ("T", 17),
        ("1", 18), ("2", 19), ("3", 20), ("4", 21), ("6", 22), ("5", 23), ("Equal", 24), ("9", 25),
        ("7", 26), ("Minus", 27), ("8", 28), ("0", 29), ("BracketRight", 30), ("O", 31), ("U", 32),
        ("BracketLeft", 33), ("I", 34), ("P", 35), ("Enter", 36), ("L", 37), ("J", 38),
        ("Quote", 39), ("K", 40), ("Semicolon", 41), ("Backslash", 42), ("Comma", 43),
        ("Slash", 44), ("N", 45), ("M", 46), ("Period", 47), ("Tab", 48), ("Space", 49),
        ("Backquote", 50), ("Backspace", 51), ("Escape", 53), ("F5", 96), ("F6", 97), ("F7", 98),
        ("F3", 99), ("F8", 100), ("F9", 101), ("F11", 103), ("F13", 105), ("F16", 106),
        ("F14", 107), ("F10", 109), ("F12", 111), ("F15", 113), ("Home", 115), ("PageUp", 116),
        ("Delete", 117), ("F4", 118), ("End", 119), ("F2", 120), ("PageDown", 121), ("F1", 122),
        ("ArrowLeft", 123), ("ArrowRight", 124), ("ArrowDown", 125), ("ArrowUp", 126),
        ("CapsLock", 57), ("F17", 64), ("NumpadDecimal", 65), ("NumpadMultiply", 67),
        ("NumpadAdd", 69), ("NumLock", 71), ("NumpadDivide", 75), ("NumpadEnter", 76),
        ("NumpadSubtract", 78), ("F18", 79), ("F19", 80), ("NumpadEqual", 81), ("Numpad0", 82),
        ("Numpad1", 83), ("Numpad2", 84), ("Numpad3", 85), ("Numpad4", 86), ("Numpad5", 87),
        ("Numpad6", 88), ("Numpad7", 89), ("F20", 90), ("Numpad8", 91), ("Numpad9", 92),
        ("IntlBackslash", 10), ("Insert", 114),
    ];
    let name = name.strip_prefix("Key").or_else(|| name.strip_prefix("Digit")).unwrap_or(name);
    KEYS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, key)| *key)
}

/// The modifiers an accelerator names (`Ctrl+Alt+Space`), as event flags.
fn modifiers_of(accelerator: &str) -> u64 {
    let mut parts: Vec<&str> = accelerator.split('+').map(str::trim).collect();
    parts.pop();
    parts.iter().fold(0, |flags, part| {
        flags | match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => FLAG_CONTROL,
            "alt" | "option" => FLAG_OPTION,
            "shift" => FLAG_SHIFT,
            "super" | "cmd" | "command" | "meta" | "cmdorctrl" | "commandorcontrol" => FLAG_COMMAND,
            _ => 0,
        }
    })
}

/// Tell the gate which key starts and stops dictation, from its accelerator (`Alt+Space`).
pub fn set_trigger_key(accelerator: &str) {
    TRIGGER_MODIFIERS.store(modifiers_of(accelerator), Ordering::Relaxed);
    let key = accelerator.rsplit('+').next().unwrap_or("").trim();
    TRIGGER.store(key_number(key).unwrap_or(NO_KEY), Ordering::Relaxed);
}

/// Can the dictation shortcut's keys be watched at all? If not, a dictation is not held.
pub fn hold_key_known() -> bool {
    TRIGGER.load(Ordering::Relaxed) != NO_KEY
}

/// The tap, made the first time it is wanted and asked for again each time until macOS allows
/// one. It lives on a thread of its own, switched off until a hold switches it on.
fn tap() -> Option<Tap> {
    let mut tap = TAP.lock();
    if tap.is_none() && super::macos_ax::accessibility_trusted() {
        let (made, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe {
            use core_foundation_sys::mach_port::CFMachPortCreateRunLoopSource;
            use core_foundation_sys::runloop::{
                kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun,
            };
            let events = (1u64 << KEY_DOWN) | (1 << KEY_UP) | (1 << FLAGS_CHANGED);
            let port = CGEventTapCreate(SESSION_EVENT_TAP, 0, 0, events, on_key, std::ptr::null_mut());
            if port.is_null() {
                let _ = made.send(None);
                return;
            }
            CGEventTapEnable(port, false);
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port as _, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            TAP_PORT.store(port as *mut c_void, Ordering::SeqCst);
            let _ = made.send(Some(Tap(port)));
            CFRunLoopRun();
        });
        *tap = wait.recv().ok().flatten();
    }
    *tap
}

/// Switch the tap on for hold number `serial`. False when there is none to switch on.
fn tap_on(serial: u64) -> bool {
    let Some(Tap(port)) = tap() else { return false };
    let mut wanted_for = TAP_FOR.lock();
    *wanted_for = serial;
    TAP_WANTED.store(true, Ordering::SeqCst);
    unsafe { CGEventTapEnable(port, true) };
    true
}

/// Switch it off again - unless a newer hold has it by now.
fn tap_off(serial: u64) {
    let mut wanted_for = TAP_FOR.lock();
    if *wanted_for == serial {
        *wanted_for = 0;
        TAP_WANTED.store(false, Ordering::SeqCst);
        let port = TAP_PORT.load(Ordering::SeqCst);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port as CFTypeRef, false) };
        }
    }
}

/// A held dictation has started, at the keypress: its number, by which a key coming up is
/// reported (`on_hold_key_up`); 0 if the shortcut's key is not known. From here its own key
/// types nothing, and any key of its shortcut coming up is told to the program.
pub fn hold_started() -> u64 {
    let trigger = TRIGGER.load(Ordering::Relaxed);
    if trigger == NO_KEY {
        return 0;
    }
    let modifiers = TRIGGER_MODIFIERS.load(Ordering::Relaxed);
    let serial = HOLD_SERIAL.fetch_add(1, Ordering::SeqCst) + 1;
    HOLD_KEY.store(trigger, Ordering::SeqCst);
    HOLD_MODIFIERS.store(modifiers, Ordering::SeqCst);
    let mine = swallowing(serial, trigger);
    // What is kept back is said before the tap is switched on, and not written again for this
    // hold: an outage clears it (`on_key`), and a write after the switching-on put it back -
    // over a key that had come up and gone down again unseen (Codex's third review, 2026-10-05).
    let has_tap = tap().is_some();
    SWALLOWING.store(if has_tap && key_down(trigger) { mine } else { 0 }, Ordering::SeqCst);
    HOLDING.store(serial, Ordering::SeqCst);
    if !(has_tap && tap_on(serial)) {
        let _ = SWALLOWING.compare_exchange(mine, 0, Ordering::SeqCst, Ordering::SeqCst);
        eprintln!("[hvtt] hold to talk without the event tap: the held key is not kept from other programs");
    }
    let kept_before = KEPT_BACK.load(Ordering::SeqCst);
    // Looks, as well as the tap's listening: a key let go ends the hold even with no tap. And
    // once the key is up - which can be after the dictation is over - the tap may go off.
    // Everything here is this hold's own: a newer hold's is left alone.
    std::thread::spawn(move || {
        while key_down(trigger) {
            if !modifiers_down(modifiers) {
                hold_key_came_up(serial);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        hold_key_came_up(serial);
        let _ = SWALLOWING.compare_exchange(mine, 0, Ordering::SeqCst, Ordering::SeqCst);
        tap_off(serial);
        let kept = KEPT_BACK.load(Ordering::SeqCst).wrapping_sub(kept_before);
        eprintln!("[hvtt] hold to talk: {kept} repeat(s) of its key kept from other programs");
    });
    serial
}

/// Asked once the recording exists: has a key of the held shortcut already come up - in the
/// moment the microphone took to open? The caller ends the dictation itself; a report of the
/// same thing, if one was also made, is then one too many and ignored.
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

/// Should the tap keep this key event from every program? Only the key a held dictation is still
/// holding down (`swallowing`; 0 for none) *repeating* under his finger. A fresh press of it is
/// never kept back: it means the key came up unseen - the tap was off for a moment - and he has
/// pressed it again (Codex's review, 2026-10-05).
fn swallowed(key: u32, repeat: bool, his: bool, swallowing: u64) -> bool {
    repeat && his && swallowing != 0 && key as u64 == swallowing & 0xFFFF
}

/// The tap: every key press, release and modifier change, before any program sees it - while a
/// hold has it switched on. Returning nothing keeps the event from everyone.
extern "C" fn on_key(_proxy: *mut c_void, kind: u32, event: CFTypeRef, _user: *mut c_void) -> CFTypeRef {
    match kind {
        // macOS switches a tap off when it is slow to answer, or on some input. Back on, if a
        // hold still wants it.
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => {
            // What happened meanwhile went unseen - the key may have come up and gone down
            // again - so nothing is kept back any more for the hold that was on (Codex's second
            // review, 2026-10-05). Letting go is still noticed, by looking.
            SWALLOWING.store(0, Ordering::SeqCst);
            let port = TAP_PORT.load(Ordering::SeqCst);
            if TAP_WANTED.load(Ordering::SeqCst) && !port.is_null() {
                unsafe { CGEventTapEnable(port as CFTypeRef, true) };
            }
        }
        KEY_DOWN | KEY_UP => {
            let key = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) } as u32;
            // From the keyboard - not the paste keys this program presses, nor another's.
            let his = unsafe { CGEventGetIntegerValueField(event, FIELD_SOURCE_STATE) } == HID_SYSTEM_STATE as i64;
            let swallowing = SWALLOWING.load(Ordering::SeqCst);
            let repeat =
                kind == KEY_DOWN && unsafe { CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) } != 0;
            if swallowed(key, repeat, his, swallowing) {
                KEPT_BACK.fetch_add(1, Ordering::SeqCst);
                return std::ptr::null();
            }
            // Up - or pressed afresh, so it came up unseen: its presses go through again, at
            // once, and the hold it belonged to is over.
            let fresh = kind == KEY_DOWN && !repeat;
            if (kind == KEY_UP || fresh) && his {
                // Only the entry just read: a newer hold's stays.
                if swallowing != 0 && key as u64 == swallowing & 0xFFFF {
                    let _ = SWALLOWING.compare_exchange(swallowing, 0, Ordering::SeqCst, Ordering::SeqCst);
                }
                let holding = HOLDING.load(Ordering::SeqCst);
                if holding != 0 && key == HOLD_KEY.load(Ordering::SeqCst) && (fresh || !key_down(key)) {
                    hold_key_came_up(holding);
                }
            }
        }
        FLAGS_CHANGED => {
            let holding = HOLDING.load(Ordering::SeqCst);
            let wanted = HOLD_MODIFIERS.load(Ordering::SeqCst);
            // The event says a modifier is up; the keyboard itself must say so too. Any program
            // can post a modifier event (Codex's review, 2026-10-05).
            if holding != 0 && unsafe { CGEventGetFlags(event) } & wanted != wanted && !modifiers_down(wanted) {
                hold_key_came_up(holding);
            }
        }
        _ => {}
    }
    event
}

/// The input half of the gate, from what was counted since the keypress and what the gate
/// expects: every click on Huck's own windows, and the keys typed inside Huck's box plus the stop
/// press - when the shortcut ended the dictation, not the Send button (`box_input`).
fn input_moved(counted: Counts, expected_keys: u32) -> Option<&'static str> {
    if counted.clicks != counted.own_clicks {
        return Some("clicked");
    }
    match counted.keys.cmp(&expected_keys) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some("stop-key-not-seen"),
        std::cmp::Ordering::Greater => Some("typed-or-repeated"),
    }
}

/// Whether the key presses counted reach what the gate expects, the stop press included. If they
/// do not, the count was not running - or a key never arrived - whatever else was counted.
fn stop_seen(counted: Counts, expected_keys: u32) -> bool {
    counted.keys >= expected_keys
}

/// The same-window rule, apart from the system calls. `strict` is the old verdict (anything at
/// all refuses); `stop_seen` is the key count reaching what the gate expects - the stop press
/// included - which proves the count ran; `place` is asked only when it matters. (Codex's first
/// review of the Windows twin, 2026-09-29: a click used to hide a missing stop press, because the
/// click was reported before the keys were looked at.)
///
/// The forgiving places do not rest on the counts: `Caret` pastes where the caret is whatever he
/// did, and `SameBox` asks Accessibility directly. The counts decide only what is refused, and
/// `stop_seen` is a check that they were running at all - not proof of which key was pressed.
fn after_moving(
    strict: Option<&'static str>,
    stop_seen: bool,
    place: impl FnOnce() -> Place,
) -> Option<&'static str> {
    let why = strict?;
    if !stop_seen {
        return Some("stop-key-not-seen");
    }
    match place() {
        Place::Caret | Place::SameBox => None,
        Place::AnotherBox => Some("another-box"),
        Place::Unknown => Some(why),
    }
}

/// Nothing moved while the same-box check ran: the same app and window still in front and holding
/// the keyboard, and no click or key press since the counts were first read. Asking Accessibility
/// takes as long as the other app takes to answer, and in that time he could have clicked or
/// switched away. (Codex's third review of the Windows twin, 2026-09-29.) `front_moved` is
/// `FocusStamp::front_moved`, read again.
fn settled(before: Counts, after: Counts, front_moved: Option<&'static str>) -> Option<&'static str> {
    front_moved.or((before != after).then_some("moved-during-check"))
}

/// Press Cmd+V in whatever has keyboard focus. Every caller decides first that focus is right.
///
/// Immediate, even with the shortcut's keys still held. The keystroke carries exactly one
/// modifier, Command, from a private event source, so Control or Option under his fingers never
/// turn it into another command. Measured 2026-09-25 in TextEdit, where a leaked Option makes
/// Cmd+V "Paste Style" and inserts nothing: with Control + Option held this pasted cleanly, and a
/// deliberate Option + Cmd + V inserted nothing - so the check could see a leak. It used to wait
/// for the keys to lift, which read as lag, and gave up silently after 1.5 s.
///
/// The one key that must be up is **V itself**: macOS drops a synthetic V while the real V is
/// held, so a shortcut ending in V pastes on its release, not its press.
pub fn press_paste() -> Result<(), DeliveryError> {
    unsafe {
        let source = CGEventSourceCreate(PRIVATE_STATE);
        let result = (|| {
            for down in [true, false] {
                let event = CGEventCreateKeyboardEvent(source, KEY_V, down);
                if event.is_null() {
                    return Err(DeliveryError::Other("could not create the paste keystroke".into()));
                }
                CGEventSetFlags(event, FLAG_COMMAND);
                CGEventPost(HID_EVENT_TAP, event);
                CFRelease(event);
            }
            Ok(())
        })();
        if !source.is_null() {
            CFRelease(source);
        }
        result
    }
}

/// Paste `text` from the normal clipboard, then put back whatever was there. The app reads the
/// clipboard when it handles the keystroke, a moment later, so the give-back waits for it.
pub fn paste_borrowing_clipboard(text: &str) -> Result<(), DeliveryError> {
    let borrowed = crate::clip::huck::borrow_general(text)
        .map_err(DeliveryError::Other)?;
    let pressed = press_paste();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = borrowed.give_back();
    });
    pressed
}

/// Hand the keyboard back to the app he was dictating into, after he fixed words in Huck's box.
/// Only Huck's box had it, and only because he clicked into it; the paste gate still checks that
/// the same window is in front before anything is pasted.
pub fn activate(pid: i32) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    let Some(class) = AnyClass::get(c"NSRunningApplication") else { return };
    unsafe {
        let app: *mut AnyObject = msg_send![class, runningApplicationWithProcessIdentifier: pid];
        if let Some(app) = app.as_ref() {
            // NSApplicationActivateIgnoringOtherApps; macOS 14 ignores it, harmlessly.
            let _: Bool = msg_send![app, activateWithOptions: 2usize];
        }
    }
}

/// The app whose ordinary window is in front - the one he was in before touching Huck's box,
/// which floats above every ordinary window.
pub fn front_app() -> Option<i32> {
    front_window().map(|(pid, _)| pid)
}

/// The pid of the active application - the one the keyboard is going to - or `None` when macOS
/// names none.
///
/// The window list above sees only ordinary windows, so a launcher or alert panel that takes the
/// keyboard leaves his window "in front" behind it; its *app* is the active one. Measured
/// 2026-09-29 with a throwaway floating panel while VS Code was in front: the front layer-0
/// window stayed VS Code's, and this changed to the panel's app within 100 ms. Accessibility
/// cannot stand in for it: with an Electron app in front its focused-application query answers
/// "no value", so a check built on it would refuse every paste into VS Code.
///
/// Read from a worker thread; the main thread's run loop keeps it current. **Not seen by this
/// alone:** a panel that takes the keyboard *without* activating its app (a non-activating
/// `NSPanel`) leaves the active app unchanged - `macos_ax::focused_app_pid` catches that one
/// (see `keyboard_verdict`).
pub fn active_app() -> Option<i32> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let class = AnyClass::get(c"NSWorkspace")?;
    unsafe {
        let workspace: *mut AnyObject = msg_send![class, sharedWorkspace];
        let app: *mut AnyObject = msg_send![workspace.as_ref()?, frontmostApplication];
        let pid: i32 = msg_send![app.as_ref()?, processIdentifier];
        (pid > 0).then_some(pid)
    }
}

/// Wait, up to `limit`, for the keyboard to be back with `pid` - after `activate`, which is
/// asynchronous. The gate's own condition (`keyboard_verdict`), so what is waited for is exactly
/// what will be checked: the active app, and no other app named by Accessibility.
pub fn wait_for_keyboard(pid: i32, limit: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if keyboard_verdict(pid, active_app(), super::macos_ax::focused_app_pid()).is_none() {
            return true;
        }
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Whether any mouse button is held right now, anywhere on screen.
pub fn mouse_is_down() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;
    let Some(class) = AnyClass::get(c"NSEvent") else { return false };
    let buttons: usize = unsafe { msg_send![class, pressedMouseButtons] };
    buttons != 0
}

/// Paste into the field that was focused at the keypress, if - and only if - it still is.
pub struct PasteDestination {
    stamp: FocusStamp,
    label: String,
    /// Huck's own clipboard is chosen, so the normal one does not hold the words yet.
    borrow: bool,
    /// What a click or key press inside the same app and window may be forgiven by.
    rule: SameWindow,
}

/// After a click or key press inside the same app and window, may the paste still go?
pub enum SameWindow {
    /// Yes, where the caret is. Chrome and Electron apps (VS Code, Slack) either name no box or
    /// share one window between the page and the address bar, so nothing can say which box the
    /// caret is in now. Decided with him 2026-09-29: the words go where it is - a browser's
    /// address bar included - typed there, never sent, and still on the clipboard. Also the rule
    /// for any app that names no text box.
    Caret,
    /// Only if Accessibility's focused element is still the box from the keypress.
    SameBox(super::macos_ax::AxElement),
}

/// Where the caret is after he clicked or typed inside the same app and window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Accessibility could not say.
    Unknown,
    Caret,
    SameBox,
    AnotherBox,
}

impl PasteDestination {
    /// Forgives clicks and typing where the caret is, until told otherwise (`with_field`,
    /// `forgiving`): an app that names no text box is treated like VS Code.
    pub fn new(stamp: FocusStamp, label: String, borrow: bool) -> Self {
        PasteDestination { stamp, label, borrow, rule: SameWindow::Caret }
    }

    /// Remember the text box itself, so that clicking elsewhere and back into it still sends.
    /// An app that names no text box keeps the caret rule.
    pub fn with_field(mut self, field: Option<super::macos_ax::AxElement>) -> Self {
        if let Some(field) = field.filter(|f| f.is_text_entry()) {
            self.rule = SameWindow::SameBox(field);
        }
        self
    }

    /// Forgive clicks and typing inside the same window by this rule, whatever the app names.
    pub fn forgiving(mut self, rule: SameWindow) -> Self {
        self.rule = rule;
        self
    }

    fn place(&self) -> Place {
        match &self.rule {
            SameWindow::Caret => Place::Caret,
            SameWindow::SameBox(field) => {
                use hvtt_core::pinning::FocusSource;
                match super::macos_ax::AxFocusSource.focused_now() {
                    Some(now) if now.same_as(field) => Place::SameBox,
                    Some(_) => Place::AnotherBox,
                    None => Place::Unknown,
                }
            }
        }
    }

    /// Why a paste would not land in his box, or `None` when it would.
    ///
    /// Clicks or typing since the keypress may have moved the caret to another box. Decided with
    /// him 2026-09-29: he clicks away and back into his box before sending, and expects it to
    /// arrive. So within the same app and window, with that app still holding the keyboard
    /// (checked first, by `front_moved`):
    /// - when the app names its focused box, the paste goes only if it is the very box from the
    ///   keypress;
    /// - when it names none - VS Code, even in Screen Reader Optimized mode - or it is Chrome or
    ///   another Chromium app, the paste goes where the caret now is. If he left it in another
    ///   box of that window, the words land there: inserted, never sent, and still on the
    ///   clipboard.
    ///
    /// Another app or window, another app holding the keyboard (a launcher's panel), or the stop
    /// press not seen - whatever else he clicked - still refuses.
    fn gone(&self) -> Option<&'static str> {
        // The counts first: anything he does while the slow questions below are being asked
        // shows up as a difference in the last re-read.
        let counted = self.stamp.counted();
        if let Some(why) = self.stamp.fast_moved() {
            return Some(why);
        }
        // Slow: Accessibility answers as fast as the other app does. Asked here, never last.
        if let Some(why) = self.stamp.named_elsewhere() {
            return Some(why);
        }
        let (page_clicks, expected_keys) = super::box_input::expected(true);
        let strict = input_moved(counted, expected_keys);
        let verdict = after_moving(strict, stop_seen(counted, expected_keys), || self.place());
        // Every log line comes before the re-read below: nothing slow may sit between the last
        // check and the keystroke. (Codex's fourth review of the Windows twin, 2026-09-29.)
        if let Some(why) = strict {
            if why == "clicked" {
                eprintln!(
                    "[hvtt] paste gate: {} click(s) since the keypress, {} on Huck's windows \
                     ({page_clicks} seen by the page)",
                    counted.clicks, counted.own_clicks
                );
            }
            if verdict.is_some() {
                return verdict;
            }
            eprintln!("[hvtt] paste gate: {why}, same app and window - forgiven if nothing moves now");
        }
        // Last, on every path that would paste, and fast: the window, the active app and the
        // counters, read again, the counters last. Anything that moved while Accessibility was
        // asked refuses. (Codex's third review of 0.1.6: the counters used to be re-read *before*
        // the Accessibility question, and the nothing-moved path did not re-read at all.)
        let front = self.stamp.fast_moved();
        settled(counted, self.stamp.counted(), front)
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
        // (Codex's fourth review of the Windows twin, 2026-09-29.)
        let borrowed = match self.borrow {
            true => Some(crate::clip::huck::borrow_general(text).map_err(DeliveryError::Other)?),
            false => None,
        };
        let pressed = match self.gone() {
            Some(_) => Err(DeliveryError::DestinationLost),
            None => press_paste(),
        };
        if let Some(borrowed) = borrowed {
            // The app reads the clipboard when it handles the keystroke, a moment later. Nothing
            // was pasted if the last check refused: his clipboard goes back at once.
            let wait = if pressed.is_ok() {
                std::time::Duration::from_millis(500)
            } else {
                std::time::Duration::ZERO
            };
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

    fn counts(clicks: u32, own_clicks: u32, keys: u32) -> Counts {
        Counts { clicks, own_clicks, keys }
    }

    #[test]
    fn only_the_single_stop_press_passes_the_key_gate() {
        assert_eq!(input_moved(counts(0, 0, 1), 1), None);
        assert_eq!(input_moved(counts(0, 0, 0), 1), Some("stop-key-not-seen"));
        assert_eq!(input_moved(counts(0, 0, 2), 1), Some("typed-or-repeated"));
        // Six keys typed in the box, then the shortcut; or the Send button and no stop press.
        assert_eq!(input_moved(counts(0, 0, 7), 7), None);
        assert_eq!(input_moved(counts(0, 0, 8), 7), Some("typed-or-repeated"));
        assert_eq!(input_moved(counts(0, 0, 0), 0), None);
    }

    #[test]
    fn only_clicks_on_huck_s_own_windows_pass_the_click_gate() {
        assert_eq!(input_moved(counts(3, 3, 1), 1), None, "Pause, Resume and Send in the box");
        assert_eq!(input_moved(counts(4, 3, 1), 1), Some("clicked"), "one click elsewhere");
        assert_eq!(input_moved(counts(2, 3, 1), 1), Some("clicked"), "counts that do not add up");
    }

    #[test]
    fn the_system_counters_may_wrap() {
        let before = counts(u32::MAX, 0, u32::MAX);
        assert_eq!(counts(0, 0, 0).since(before), counts(1, 0, 1));
        assert_eq!(counts(5, 2, 9).since(counts(5, 2, 9)), counts(0, 0, 0));
    }

    #[test]
    fn clicking_away_and_back_in_the_same_window_still_sends() {
        // Decided with him 2026-09-29, where something can say where the caret went.
        let never = || -> Place { panic!("not asked when nothing moved") };
        assert_eq!(after_moving(None, true, never), None, "nothing moved");
        assert_eq!(after_moving(Some("clicked"), true, || Place::Caret), None, "VS Code, Chrome");
        assert_eq!(after_moving(Some("clicked"), true, || Place::SameBox), None, "back in his box");
        assert_eq!(after_moving(Some("typed-or-repeated"), true, || Place::SameBox), None);
        assert_eq!(after_moving(Some("clicked"), true, || Place::AnotherBox), Some("another-box"));
        assert_eq!(
            after_moving(Some("clicked"), true, || Place::Unknown),
            Some("clicked"),
            "Accessibility could not say: refuse, and give the real reason"
        );
    }

    #[test]
    fn a_click_never_hides_a_missing_stop_press() {
        // Codex's finding on the Windows twin: the click was reported before the keys were looked
        // at, so a stop press that never arrived - the count was not running - was forgiven.
        let counted = counts(1, 0, 0);
        let strict = input_moved(counted, 1);
        assert_eq!(strict, Some("clicked"));
        for place in [Place::Caret, Place::SameBox] {
            assert_eq!(
                after_moving(strict, stop_seen(counted, 1), || place),
                Some("stop-key-not-seen"),
                "{place:?}"
            );
        }
        // Sent with the button, no stop press is expected, so nothing is missing.
        let sent = counts(1, 0, 0);
        assert_eq!(after_moving(input_moved(sent, 0), stop_seen(sent, 0), || Place::Caret), None);
    }

    #[test]
    fn the_keyboard_must_still_be_with_the_app_he_dictated_into() {
        // Codex's review of 0.1.6: a launcher's panel floats above his window, so the window list
        // still says his window is in front - the active app is what changes (measured).
        // Accessibility says nothing with an Electron app in front: that is no reason to refuse.
        assert_eq!(keyboard_verdict(501, Some(501), None), None, "VS Code in front, as measured");
        assert_eq!(keyboard_verdict(501, Some(501), Some(501)), None, "a native app in front");
        assert_eq!(keyboard_verdict(501, Some(777), None), Some("keyboard-elsewhere"), "a launcher took it");
        assert_eq!(keyboard_verdict(501, Some(777), Some(777)), Some("keyboard-elsewhere"));
        assert_eq!(keyboard_verdict(501, None, None), Some("no-active-app"), "macOS names none: refuse");
        // Codex's second review: a panel that takes the keyboard without activating its app. The
        // active app is still his; Accessibility names the panel's (measured with a real panel).
        assert_eq!(keyboard_verdict(501, Some(501), Some(777)), Some("keyboard-elsewhere"));
        // The two halves, asked apart since Codex's third review: the fast one last.
        assert_eq!(active_verdict(501, Some(501)), None);
        assert_eq!(active_verdict(501, None), Some("no-active-app"));
        assert_eq!(named_verdict(501, None), None, "silence decides nothing");
        assert_eq!(named_verdict(501, Some(777)), Some("keyboard-elsewhere"));
    }

    #[test]
    fn the_active_app_can_be_read_from_a_worker_thread() {
        // Huck reads it from delivery threads. A wrong selector would abort the process here, as
        // the login-item one once did at launch; nothing is asserted about *which* app is in front.
        let pid = std::thread::spawn(active_app).join().expect("the read did not crash");
        if let Some(pid) = pid {
            assert!(pid > 0);
        }
    }

    #[test]
    fn accessibility_can_be_asked_who_has_the_keyboard_from_a_worker_thread() {
        // Nothing is asserted about the answer - with an Electron app in front it is "no value".
        // This catches a wrong attribute name or a bad reference, which would crash here.
        let named = std::thread::spawn(crate::destination::macos_ax::focused_app_pid)
            .join()
            .expect("the question did not crash");
        if let Some(pid) = named {
            assert!(pid > 0);
        }
    }

    #[test]
    fn the_shortcut_names_the_keys_a_hold_watches() {
        assert_eq!(key_number("Space"), Some(49));
        assert_eq!(key_number("V"), Some(9));
        assert_eq!(key_number("F"), Some(3), "the letter, not a function key");
        assert_eq!(key_number("F5"), Some(96));
        assert_eq!(key_number("A"), Some(0), "the Mac's key 0 is a real key");
        assert_eq!(key_number("Numpad1"), Some(83));
        assert_eq!(key_number("AudioVolumeUp"), None, "not known: that shortcut is not held");
        assert_eq!(modifiers_of("Alt+Space"), FLAG_OPTION);
        assert_eq!(modifiers_of("Alt+Ctrl+V"), FLAG_OPTION | FLAG_CONTROL);
        assert_eq!(modifiers_of("Cmd+Shift+Space"), FLAG_COMMAND | FLAG_SHIFT);
    }

    #[test]
    fn a_held_dictation_keeps_back_only_its_own_keys_presses() {
        let space = swallowing(3, 49);
        assert!(swallowed(49, true, true, space), "Space repeating under his thumb");
        assert!(!swallowed(49, false, true, space), "its release, or a fresh press, goes through");
        assert!(!swallowed(9, true, true, space), "any other key is his typing");
        assert!(!swallowed(49, true, false, space), "a press this program or another made");
        assert!(!swallowed(49, true, true, 0), "no hold: Space is Space");
        // Key 0 is A on the Mac: a hold on A keeps back A, and "none" keeps back nothing.
        assert!(swallowed(0, true, true, swallowing(1, 0)));
        assert!(!swallowed(0, true, true, 0));
    }

    #[test]
    fn nothing_may_move_while_the_same_box_check_runs() {
        // Codex's third review of the Windows twin: focus could change while Accessibility was
        // being asked "same box?".
        let before = counts(2, 1, 3);
        assert_eq!(settled(before, counts(2, 1, 3), None), None);
        assert_eq!(settled(before, counts(3, 1, 3), None), Some("moved-during-check"), "a click");
        assert_eq!(settled(before, counts(2, 2, 3), None), Some("moved-during-check"), "in the box");
        assert_eq!(settled(before, counts(2, 1, 4), None), Some("moved-during-check"), "a key");
        assert_eq!(settled(before, counts(2, 1, 3), Some("app-switched")), Some("app-switched"));
        assert_eq!(settled(before, counts(2, 1, 3), Some("window-changed")), Some("window-changed"));
    }
}
