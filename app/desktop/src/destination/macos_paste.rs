//! The paste rung: for apps with no silent write path - Electron (VS Code, Slack, Discord, the
//! Claude and ChatGPT apps) and Chrome without the extension.
//!
//! A paste lands in *whatever is focused*, which is why there was no paste path at all: when the
//! field-capture spike's target died, its paste put the words in an unrelated application. So
//! this rung is gated on the one thing that makes a paste safe - **nothing has moved since the
//! keypress**. Same frontmost app, same front window, no mouse click, no typing beyond the
//! shortcut itself. Then the field focused at the keypress is still the focused field, and a
//! paste goes exactly where the dictation was aimed. If any of that changed, it refuses, and the
//! words wait on the clipboard.
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

// Exactly one key press is allowed between capture and delivery beyond what he typed inside
// Huck's box: the stop shortcut's ordinary key (`box_input::expected`). Core Graphics' cumulative
// counter includes auto-repeat and cannot identify which key repeated, so a held stop shortcut
// refuses the paste instead of granting extra presses that could hide real typing. The words
// remain on the chosen clipboard in every refusal.

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

/// What was in front, and how much he had touched, at the moment the shortcut arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusStamp {
    pid: i32,
    window: i64,
    clicks: u32,
    own_clicks: u32,
    keys: u32,
}

impl FocusStamp {
    /// Microseconds for the counters; under a millisecond for the window list once warm.
    pub fn capture() -> Option<Self> {
        let (pid, window) = front_window()?;
        Some(FocusStamp { pid, window, clicks: clicks(), own_clicks: own_clicks(), keys: keys() })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Why a paste would no longer land in the field that was focused at the keypress.
    fn moved(&self) -> Option<&'static str> {
        match front_window() {
            None => return Some("no-front-window"),
            Some((pid, _)) if pid != self.pid => return Some("app-switched"),
            Some((_, window)) if window != self.window => return Some("window-changed"),
            Some(_) => {}
        }
        // What he did inside Huck's own windows is expected; the system counter includes
        // auto-repeat. Clicks come from the native count, keys from the box.
        let (page_clicks, expected_keys) = super::box_input::expected(true);
        let clicked = clicks().wrapping_sub(self.clicks);
        let own = own_clicks().wrapping_sub(self.own_clicks);
        if clicked != own {
            eprintln!(
                "[hvtt] paste gate: {clicked} click(s) since the keypress, {own} on Huck's windows \
                 ({page_clicks} seen by the page)"
            );
            return Some("clicked");
        }
        if let Some(why) = key_change_reason(self.keys, keys(), expected_keys) {
            return Some(why);
        }
        None
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

fn keys() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID_SYSTEM_STATE, KEY_DOWN) }
}

/// `expected` is the stop press plus any keys typed inside Huck's box, or
/// only the latter when the Send button finished the dictation.
fn key_change_reason(before: u32, after: u32, expected: u32) -> Option<&'static str> {
    match after.wrapping_sub(before).cmp(&expected) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some("stop-key-not-seen"),
        std::cmp::Ordering::Greater => Some("typed-or-repeated"),
    }
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
    /// The text box focused at the keypress, when the app says which one it is.
    field: Option<super::macos_ax::AxElement>,
}

impl PasteDestination {
    pub fn new(stamp: FocusStamp, label: String, borrow: bool) -> Self {
        PasteDestination { stamp, label, borrow, field: None }
    }

    /// Remember the text box itself, so that clicking elsewhere and back into it still sends.
    pub fn with_field(mut self, field: Option<super::macos_ax::AxElement>) -> Self {
        self.field = field.filter(|f| f.is_text_entry());
        self
    }

    /// Why a paste would not land in his box, or `None` when it would.
    ///
    /// Clicks or typing since the keypress may have moved the caret to another box. Decided with
    /// him 2026-09-29: he clicks away and back into his box before sending, and expects it to
    /// arrive. So within the same app and window (checked first, by `moved`):
    /// - when the app names its focused box, the paste goes only if it is the very box from the
    ///   keypress;
    /// - when it names none - VS Code, even in Screen Reader Optimized mode - the paste goes
    ///   where the caret now is. If he left it in another box of that window, the words land
    ///   there: inserted, never sent, and still on the clipboard.
    ///
    /// Another app or window, or the stop press not seen, still refuses.
    fn gone(&self) -> Option<&'static str> {
        let why = self.stamp.moved()?;
        if !matches!(why, "clicked" | "typed-or-repeated") {
            return Some(why);
        }
        let Some(field) = &self.field else {
            eprintln!("[hvtt] paste gate: {why}, same window - sending where the caret is");
            return None;
        };
        use hvtt_core::pinning::FocusSource;
        match super::macos_ax::AxFocusSource.focused_now() {
            Some(now) if now.same_as(field) => {
                eprintln!("[hvtt] paste gate: {why}, but back in the same box - sending");
                None
            }
            _ => Some("another-box"),
        }
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

    fn deliver(&self, text: &str) -> Result<(), DeliveryError> {
        // Checked again last thing: transcription took time in which he could have clicked away.
        if self.gone().is_some() {
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
    use super::key_change_reason;

    #[test]
    fn only_the_single_stop_press_passes_the_key_gate() {
        assert_eq!(key_change_reason(10, 11, 1), None);
        assert_eq!(key_change_reason(10, 10, 1), Some("stop-key-not-seen"));
        assert_eq!(key_change_reason(10, 12, 1), Some("typed-or-repeated"));
        assert_eq!(key_change_reason(u32::MAX, 0, 1), None, "the system counter may wrap");
        // Six keys typed in the box, then the shortcut; or the Send button and no stop press.
        assert_eq!(key_change_reason(10, 17, 7), None);
        assert_eq!(key_change_reason(10, 18, 7), Some("typed-or-repeated"));
        assert_eq!(key_change_reason(10, 10, 0), None);
    }
}
