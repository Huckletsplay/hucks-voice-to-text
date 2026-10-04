//! Completion: what happens the moment recognition finishes.
//!
//! This module exists so the product rule is enforced by code rather than by remembering to do
//! things in the right order in the UI. The order is fixed and not configurable:
//!
//! 1. **Persist a recovery draft to disk.** Survives a crash of Huck itself.
//! 2. **Copy to the clipboard.** Always, unconditionally, before anything can go wrong.
//! 3. **Then** attempt delivery to the pinned destination, if there is one.
//!
//! Nothing here clears a transcript. There is deliberately no code path that does, on either
//! success or failure: the transcript is sacred.

use crate::drafts::DraftStore;
use crate::transcript::Transcript;
use serde::{Deserialize, Serialize};

/// The system clipboard. Implemented by the desktop crate.
pub trait Clipboard {
    fn set_text(&self, text: &str) -> Result<(), String>;
}

/// Whether a pinned destination is still unquestionably the one that was pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liveness {
    /// The original target, still reachable.
    Alive,
    /// Gone, or no longer provably the same thing. `reason` is for the log, not the user.
    Dead { reason: String },
}

impl Liveness {
    pub fn dead(reason: impl Into<String>) -> Self {
        Liveness::Dead { reason: reason.into() }
    }
    pub fn is_alive(&self) -> bool {
        matches!(self, Liveness::Alive)
    }
}

/// A pinned text destination in another application.
///
/// This is the OS-native seam: macOS Accessibility for native apps, a browser extension over
/// Native Messaging for Chromium. Both spikes converged on the same rules, and the trait exists
/// to make those rules impossible to skip:
///
/// - **[`is_alive`](Destination::is_alive) gates everything.** `complete_transcription` refuses
///   before it will call `deliver`, so an implementation cannot "just try it".
/// - **[`deliver`](Destination::deliver) must verify by reading back.** Both platforms return
///   success from writes that did nothing — Chromium for every write, WebKit for
///   `AXSelectedText`. A write that cannot be read back is a failure.
/// - **There is no re-find.** The trait offers no way to describe a target so it can be located
///   again, because a replacement field is indistinguishable from the original: after a page
///   reload every one of 18 measured attributes matched. Identity is the held reference.
/// - **Nothing here targets "the focused field".** That API does not exist, deliberately.
pub trait Destination: Send + Sync {
    /// A short human label for the indicator, e.g. `"Slack — message box"`.
    fn label(&self) -> String;

    /// Is the pinned target still the original? Checked before every delivery.
    fn is_alive(&self) -> Liveness;

    /// Write the text and confirm it landed. Implementations read the value back.
    ///
    /// `copied`: the clipboard copy `complete_transcription` has just made holds these words. It
    /// is an argument, not a question asked of the destination, so that no implementation can
    /// reach a paste without having been told: a paste sends whatever the clipboard holds, and
    /// with the copy failed that is what he had copied before - into his text box, reported as
    /// sent. A destination that writes the text itself may still do so; **any step of it that
    /// presses the paste keys on the strength of that copy must refuse when `copied` is false**,
    /// including a fallback inside a destination that normally writes directly (Codex's sixth
    /// review of the 0.1.8 candidate: the first guard asked only the outermost destination).
    fn deliver(&self, text: &str, copied: bool) -> Result<(), DeliveryError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum DeliveryError {
    /// The field is gone, or its reference went stale and could not be re-found.
    DestinationLost,
    /// A silent write was not available and taking the foreground is never acceptable.
    WouldStealForeground,
    /// The write reported success but the text could not be read back. Both platforms do this,
    /// so it is a first-class outcome rather than an edge case.
    NotVerified,
    /// The destination did not answer. Silence is refusal, never "probably fine".
    NoResponse,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum DeliveryOutcome {
    /// No destination was pinned. Expected in milestone 1, and not a failure.
    NotAttempted,
    Delivered { label: String },
    Failed {
        label: String,
        error: DeliveryError,
        /// Diagnostic detail for the log. Never shown to the user verbatim.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionReport {
    /// The text as it stands. Always present, whatever else happened.
    pub text: String,
    /// Whether the clipboard now holds the transcript.
    pub clipboard_ok: bool,
    /// Set only if the clipboard write itself failed — the one case where the user must act.
    pub clipboard_error: Option<String>,
    /// Where the recovery draft was written, if it could be written.
    pub draft_path: Option<String>,
    pub delivery: DeliveryOutcome,
    /// The sentence to show the user. Never blank.
    pub message: String,
}

impl CompletionReport {
    /// True when the words are safe somewhere other than the composer.
    pub fn text_is_safe(&self) -> bool {
        self.clipboard_ok || self.draft_path.is_some()
    }
}

fn paste_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Press Cmd+V to paste it."
    } else {
        "Press Ctrl+V to paste it."
    }
}

/// Run the completion sequence. See the module docs for the guaranteed order.
///
/// `destination` is `None` in milestone 1. Passing `Some` does not change steps 1 and 2 — a
/// pinned destination can never cause the clipboard copy to be skipped.
pub fn complete_transcription(
    transcript: &Transcript,
    clipboard: &dyn Clipboard,
    destination: Option<&dyn Destination>,
    drafts: Option<&DraftStore>,
) -> CompletionReport {
    let text = transcript.text().to_string();

    // 1. Recovery draft first: it is the only copy that survives Huck crashing.
    let draft_path = drafts
        .and_then(|d| d.save(&text).ok())
        .map(|p| p.display().to_string());

    // 2. Clipboard, unconditionally, before any delivery can fail.
    let (clipboard_ok, clipboard_error) = match clipboard.set_text(&text) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e)),
    };

    // 3. Only now, delivery.
    let delivery = match destination {
        None => DeliveryOutcome::NotAttempted,
        Some(d) => {
            let label = d.label();
            // THE GATE. Liveness is checked here, in shared code, so no platform layer can
            // skip it — and an empty transcript is never delivered anywhere.
            match d.is_alive() {
                Liveness::Dead { reason } => DeliveryOutcome::Failed {
                    label,
                    error: DeliveryError::DestinationLost,
                    detail: Some(reason),
                },
                Liveness::Alive if text.trim().is_empty() => DeliveryOutcome::NotAttempted,
                Liveness::Alive => match d.deliver(&text, clipboard_ok) {
                    Ok(()) => DeliveryOutcome::Delivered { label },
                    Err(error) => DeliveryOutcome::Failed { label, error, detail: None },
                },
            }
        }
    };

    let message = build_message(&delivery, clipboard_ok, clipboard_error.as_deref(), &draft_path);

    CompletionReport { text, clipboard_ok, clipboard_error, draft_path, delivery, message }
}

fn build_message(
    delivery: &DeliveryOutcome,
    clipboard_ok: bool,
    clipboard_error: Option<&str>,
    draft_path: &Option<String>,
) -> String {
    // The clipboard failing is the only situation where the words might be hard to reach, so it
    // outranks whatever delivery did.
    if !clipboard_ok {
        let where_it_is = match draft_path {
            Some(p) => format!(" It is saved at {p}."),
            None => String::new(),
        };
        return format!(
            "Could not copy to the clipboard ({}). Your transcription is still in the composer \
             below — copy it before closing.{}",
            clipboard_error.unwrap_or("unknown error"),
            where_it_is
        );
    }

    match delivery {
        DeliveryOutcome::NotAttempted => {
            format!("Copied to the clipboard. {}", paste_hint())
        }
        DeliveryOutcome::Delivered { label } => {
            format!("Sent to {label}. Also copied to the clipboard.")
        }
        DeliveryOutcome::Failed { error, .. } => {
            let why = match error {
                DeliveryError::DestinationLost => "The linked text field could not be reached.",
                DeliveryError::WouldStealForeground => {
                    "The linked text field needs to be in front to receive text, and Huck will \
                     not pull you out of what you are doing."
                }
                DeliveryError::NotVerified => {
                    "Huck could not confirm the text arrived, so it is not claiming it did."
                }
                DeliveryError::NoResponse => "The linked text field did not respond.",
                DeliveryError::Other(_) => "The linked text field could not be reached.",
            };
            format!("{why} Your transcription is on the clipboard. {}", paste_hint())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeClipboard {
        written: RefCell<Vec<String>>,
        fail: bool,
    }
    impl FakeClipboard {
        fn working() -> Self { FakeClipboard { written: RefCell::new(vec![]), fail: false } }
        fn broken() -> Self { FakeClipboard { written: RefCell::new(vec![]), fail: true } }
        fn last(&self) -> Option<String> { self.written.borrow().last().cloned() }
    }
    impl Clipboard for FakeClipboard {
        fn set_text(&self, text: &str) -> Result<(), String> {
            if self.fail { return Err("clipboard unavailable".into()); }
            self.written.borrow_mut().push(text.to_string());
            Ok(())
        }
    }

    /// Records whether `deliver` was reached, which is how the liveness gate is proven.
    struct SpyDestination {
        liveness: Liveness,
        result: Option<DeliveryError>,
        deliver_calls: RefCell<Vec<String>>,
        /// Delivers by pressing the paste keys, relying on the clipboard copy.
        pastes: bool,
        /// What each call was told about the copy.
        told_copied: RefCell<Vec<bool>>,
    }
    impl SpyDestination {
        fn alive() -> Self {
            SpyDestination { liveness: Liveness::Alive, result: None,
                             deliver_calls: RefCell::new(vec![]), pastes: false,
                             told_copied: RefCell::new(vec![]) }
        }
        fn pasting() -> Self {
            SpyDestination { pastes: true, ..SpyDestination::alive() }
        }
        fn dead(reason: &str) -> Self {
            SpyDestination { liveness: Liveness::dead(reason), ..SpyDestination::alive() }
        }
        fn failing(e: DeliveryError) -> Self {
            SpyDestination { result: Some(e), ..SpyDestination::alive() }
        }
        fn was_written_to(&self) -> bool { !self.deliver_calls.borrow().is_empty() }
    }
    // The fakes are single-threaded; the trait bound is for real implementations.
    unsafe impl Send for SpyDestination {}
    unsafe impl Sync for SpyDestination {}
    impl Destination for SpyDestination {
        fn label(&self) -> String { "Slack - message box".into() }
        fn is_alive(&self) -> Liveness { self.liveness.clone() }
        fn deliver(&self, text: &str, copied: bool) -> Result<(), DeliveryError> {
            self.told_copied.borrow_mut().push(copied);
            // As the real paste rung does: no copy, no paste.
            if self.pastes && !copied {
                return Err(DeliveryError::Other("nothing of his to paste".into()));
            }
            self.deliver_calls.borrow_mut().push(text.to_string());
            match self.result.clone() { None => Ok(()), Some(e) => Err(e) }
        }
    }

    // ---------------------------------------------------------------- the product rule

    #[test]
    fn clipboard_always_gets_the_text_when_no_destination_is_pinned() {
        let clip = FakeClipboard::working();
        let r = complete_transcription(&Transcript::settled("the quick brown fox"), &clip, None, None);
        assert!(r.clipboard_ok);
        assert_eq!(clip.last().as_deref(), Some("the quick brown fox"));
        assert_eq!(r.delivery, DeliveryOutcome::NotAttempted);
    }

    #[test]
    fn clipboard_still_gets_the_text_when_delivery_fails() {
        let clip = FakeClipboard::working();
        let dest = SpyDestination::failing(DeliveryError::DestinationLost);
        let r = complete_transcription(
            &Transcript::settled("words that must not be lost"), &clip, Some(&dest), None);

        assert_eq!(clip.last().as_deref(), Some("words that must not be lost"));
        assert_eq!(r.text, "words that must not be lost");
        assert!(r.text_is_safe());
        assert!(r.message.contains("on the clipboard"), "got: {}", r.message);
    }

    #[test]
    fn clipboard_gets_the_text_even_when_delivery_succeeds() {
        let clip = FakeClipboard::working();
        let dest = SpyDestination::alive();
        let r = complete_transcription(&Transcript::settled("delivered and copied"), &clip,
                                       Some(&dest), None);
        assert_eq!(clip.last().as_deref(), Some("delivered and copied"));
        assert!(matches!(r.delivery, DeliveryOutcome::Delivered { .. }));
    }

    #[test]
    fn a_broken_clipboard_is_reported_and_the_text_is_still_returned() {
        let clip = FakeClipboard::broken();
        let r = complete_transcription(&Transcript::settled("still here"), &clip, None, None);
        assert!(!r.clipboard_ok);
        assert_eq!(r.text, "still here", "the transcript is never dropped");
        assert!(r.message.contains("still in the composer"), "got: {}", r.message);
    }

    #[test]
    fn a_paste_is_never_made_from_a_clipboard_that_did_not_take_the_words() {
        // The paste rung presses the paste keys and relies on the copy made just before it. With
        // that copy failed, the keys would put whatever he had copied earlier into his text box,
        // and the box would say "Sent". Every delivery is told whether the copy was made.
        let clip = FakeClipboard::broken();
        let dest = SpyDestination::pasting();
        let r = complete_transcription(&Transcript::settled("not his old clipboard"), &clip,
                                       Some(&dest), None);
        assert_eq!(*dest.told_copied.borrow(), vec![false], "told the copy failed");
        assert!(!dest.was_written_to(), "nothing may be pasted");
        assert!(matches!(r.delivery, DeliveryOutcome::Failed { .. }), "and nothing is called sent");
        assert!(!r.text_is_safe(), "the words are only in the box, and it must say so");

        // The same destination, with the copy made: pasted as always.
        let clip = FakeClipboard::working();
        let dest = SpyDestination::pasting();
        let r = complete_transcription(&Transcript::settled("his words"), &clip, Some(&dest), None);
        assert_eq!(*dest.told_copied.borrow(), vec![true]);
        assert!(dest.was_written_to());
        assert!(matches!(r.delivery, DeliveryOutcome::Delivered { .. }));
    }

    #[test]
    fn a_destination_that_writes_the_text_itself_does_not_need_the_clipboard() {
        let clip = FakeClipboard::broken();
        let dest = SpyDestination::alive();
        let r = complete_transcription(&Transcript::settled("typed straight in"), &clip,
                                       Some(&dest), None);
        assert!(dest.was_written_to());
        assert!(matches!(r.delivery, DeliveryOutcome::Delivered { .. }));
    }

    // ---------------------------------------------------------------- the liveness gate

    #[test]
    fn a_dead_destination_is_never_written_to() {
        // The rule the field-capture spike violated: with the target gone it fell through to a
        // paste and put the text in an unrelated application. `deliver` must not even be called.
        let clip = FakeClipboard::working();
        let dest = SpyDestination::dead("element-detached");

        let r = complete_transcription(&Transcript::settled("do not lose these"), &clip,
                                       Some(&dest), None);

        assert!(!dest.was_written_to(), "deliver() must not be reached for a dead destination");
        assert!(matches!(r.delivery,
            DeliveryOutcome::Failed { error: DeliveryError::DestinationLost, .. }));
        assert_eq!(clip.last().as_deref(), Some("do not lose these"));
    }

    #[test]
    fn a_look_alike_replacement_is_not_rebound_to() {
        // After a page reload every measured attribute of the replacement field matched the
        // original. The only safe answer is that the destination reports itself dead, and that
        // nothing tries to find a substitute.
        let clip = FakeClipboard::working();
        let dest = SpyDestination::dead("replaced-by-identical-element");

        let r = complete_transcription(&Transcript::settled("private message"), &clip,
                                       Some(&dest), None);

        assert!(!dest.was_written_to(), "a replacement must never receive the text");
        assert!(r.message.contains("could not be reached"), "got: {}", r.message);
    }

    #[test]
    fn an_unverified_write_is_a_failure_not_a_success() {
        // Chromium returns success from writes that do nothing; WebKit does it for
        // AXSelectedText. Reporting those as delivered would lose the user's words silently.
        let clip = FakeClipboard::working();
        let dest = SpyDestination::failing(DeliveryError::NotVerified);

        let r = complete_transcription(&Transcript::settled("did this arrive"), &clip,
                                       Some(&dest), None);

        assert!(matches!(r.delivery,
            DeliveryOutcome::Failed { error: DeliveryError::NotVerified, .. }));
        assert!(r.message.contains("could not confirm"), "got: {}", r.message);
        assert!(r.clipboard_ok);
    }

    #[test]
    fn silence_from_a_destination_is_refusal() {
        let clip = FakeClipboard::working();
        let dest = SpyDestination::failing(DeliveryError::NoResponse);
        let r = complete_transcription(&Transcript::settled("hello"), &clip, Some(&dest), None);
        assert!(matches!(r.delivery,
            DeliveryOutcome::Failed { error: DeliveryError::NoResponse, .. }));
        assert!(r.clipboard_ok);
    }

    #[test]
    fn an_empty_transcript_is_never_delivered_anywhere() {
        let clip = FakeClipboard::working();
        let dest = SpyDestination::alive();
        let r = complete_transcription(&Transcript::empty(), &clip, Some(&dest), None);
        assert!(!dest.was_written_to(), "empty text must not be pushed into someone's field");
        assert_eq!(r.delivery, DeliveryOutcome::NotAttempted);
        assert!(!r.message.is_empty());
    }

    #[test]
    fn the_clipboard_is_written_before_delivery_is_attempted() {
        // Ordering is the whole safety argument: if delivery panics or hangs, the words are
        // already reachable.
        struct OrderingDest<'a> { clip: &'a FakeClipboard }
        unsafe impl Send for OrderingDest<'_> {}
        unsafe impl Sync for OrderingDest<'_> {}
        impl Destination for OrderingDest<'_> {
            fn label(&self) -> String { "ordering".into() }
            fn is_alive(&self) -> Liveness { Liveness::Alive }
            fn deliver(&self, _t: &str, _copied: bool) -> Result<(), DeliveryError> {
                assert!(self.clip.last().is_some(),
                        "clipboard must already hold the transcript before delivery runs");
                Ok(())
            }
        }
        let clip = FakeClipboard::working();
        let dest = OrderingDest { clip: &clip };
        let r = complete_transcription(&Transcript::settled("order matters"), &clip,
                                       Some(&dest), None);
        assert!(matches!(r.delivery, DeliveryOutcome::Delivered { .. }));
    }
}
