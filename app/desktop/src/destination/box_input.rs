//! What he did inside Huck's own box during a dictation, for the paste gate.
//!
//! The paste rung is only safe while nothing has moved since the keypress, and it counts clicks
//! and key presses to know. Since 2026-09-28 the box has buttons (Pause, Send) and a text he can
//! fix before sending, so some of those clicks and keys are his work *in the box*, not a sign he
//! went somewhere else. The box counts them itself and reports them here; the gate allows exactly
//! that many and no more. A count that does not add up refuses the paste, and the words wait on
//! the clipboard - the same safe failure as before.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

static GENERATION: AtomicU64 = AtomicU64::new(0);
static CLICKS: AtomicU32 = AtomicU32::new(0);
/// Key presses, auto-repeat not counted (how the Windows hooks count).
static KEYS: AtomicU32 = AtomicU32::new(0);
/// Key presses with auto-repeat counted (how macOS's system counter counts).
static KEYS_REPEATED: AtomicU32 = AtomicU32::new(0);
/// Finished by the shortcut, whose own key press the gate expects; not by the Send button.
static STOPPED_BY_KEY: AtomicBool = AtomicBool::new(true);

/// A new dictation: nothing done in the box yet.
pub fn reset(generation: u64) {
    GENERATION.store(generation, Ordering::SeqCst);
    CLICKS.store(0, Ordering::SeqCst);
    KEYS.store(0, Ordering::SeqCst);
    KEYS_REPEATED.store(0, Ordering::SeqCst);
    STOPPED_BY_KEY.store(true, Ordering::SeqCst);
}

/// The box's running totals for this dictation. Reports for an older dictation are ignored, and
/// totals only ever grow, so a late report can never lower them.
pub fn record(generation: u64, clicks: u32, keys: u32, keys_repeated: u32) {
    if GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }
    CLICKS.fetch_max(clicks, Ordering::SeqCst);
    KEYS.fetch_max(keys, Ordering::SeqCst);
    KEYS_REPEATED.fetch_max(keys_repeated, Ordering::SeqCst);
}

pub fn set_stopped_by_key(by_key: bool) {
    STOPPED_BY_KEY.store(by_key, Ordering::SeqCst);
}

/// Clicks, and key presses, the gate should expect since the keypress: everything done in the
/// box, plus the stop shortcut's own press when that is how the dictation ended.
pub fn expected(repeats_counted: bool) -> (u32, u32) {
    let keys = if repeats_counted { &KEYS_REPEATED } else { &KEYS };
    let stop = STOPPED_BY_KEY.load(Ordering::SeqCst) as u32;
    (CLICKS.load(Ordering::SeqCst), keys.load(Ordering::SeqCst) + stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_expects_the_box_work_plus_the_stop() {
        reset(7);
        assert_eq!(expected(false), (0, 1), "a plain dictation: the stop press only");
        record(7, 2, 5, 9);
        record(7, 1, 3, 4); // a late, smaller report changes nothing
        record(6, 50, 50, 50); // an older dictation's report is ignored
        assert_eq!(expected(false), (2, 6));
        assert_eq!(expected(true), (2, 10));
        set_stopped_by_key(false);
        assert_eq!(expected(false), (2, 5), "sent with the button: no stop press");
        reset(8);
        assert_eq!(expected(false), (0, 1));
    }
}
