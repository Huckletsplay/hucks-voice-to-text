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

/// Key presses allowed between the keypress and delivery: the stop press, plus auto-repeat if
/// the shortcut is held a moment. Anything more means he typed somewhere.
const SHORTCUT_KEYS: u32 = 3;

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

/// What was in front, and how much he had touched, at the moment the shortcut arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusStamp {
    pid: i32,
    window: i64,
    clicks: u32,
    keys: u32,
}

impl FocusStamp {
    /// Microseconds for the counters; under a millisecond for the window list once warm.
    pub fn capture() -> Option<Self> {
        let (pid, window) = front_window()?;
        Some(FocusStamp { pid, window, clicks: clicks(), keys: keys() })
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
        if clicks() != self.clicks {
            return Some("clicked");
        }
        if keys().wrapping_sub(self.keys) > SHORTCUT_KEYS {
            return Some("typed");
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
        .ok_or_else(|| DeliveryError::Other("the clipboard refused the text".into()))?;
    let pressed = press_paste();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        borrowed.give_back();
    });
    pressed
}

/// Paste into the field that was focused at the keypress, if - and only if - it still is.
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
