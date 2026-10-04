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

fn keys() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID_SYSTEM_STATE, KEY_DOWN) }
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
