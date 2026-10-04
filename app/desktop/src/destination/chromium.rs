//! Chromium destination — the browser extension holds the pin, this speaks to it.
//!
//! The Accessibility API cannot write into Chromium at all: it reports the attributes as
//! settable, returns success, and does nothing. The extension can, and the spike measured a
//! 9 ms write into ChatGPT's composer with Chrome in the background.
//!
//! The safety rules are identical to the native path, and for the same reasons:
//! the extension pins a **direct DOM element reference** (never a selector), refuses when the
//! element is detached or the page navigated, and verifies every write by reading it back.
//! **Silence is refusal** — if the tab, the page or the browser is gone, nothing answers, and
//! that is treated as a lost destination rather than an assumption of success.

use crate::bridge::{Bridge, BridgeError};
use hvtt_core::pipeline::{DeliveryError, Destination, Liveness};
use std::time::Duration;

/// Deliberately short. The user is waiting, and a slow answer is indistinguishable from a lost
/// destination — both end with the transcript on the clipboard and a clear message.
const STATUS_TIMEOUT: Duration = Duration::from_millis(600);
const DELIVER_TIMEOUT: Duration = Duration::from_millis(1500);

/// An ordinary `ok` from an old extension does not prove that it enforces pin ownership.
/// Keep this check before constructing a destination: an error takes the existing paste route,
/// and cannot produce a scoped release to an extension that ignores names.
fn validate_pin_answer(answer: &serde_json::Value, token: u64) -> Result<(), String> {
    if answer.get("protocol").and_then(|v| v.as_str()) != Some("owned-pin-v1") {
        return Err("browser-extension-needs-pin-ownership".into());
    }
    if answer.get("pin").and_then(|v| v.as_u64()) != Some(token) {
        return Err("browser-extension-answered-for-another-pin".into());
    }
    Ok(())
}

pub struct ChromiumDestination {
    bridge: Bridge,
    label: String,
    /// This dictation's pin, by name (`next_pin_token`). Every later question about it carries
    /// the name, and the extension answers only for the pin that has it. Without one, "the pin"
    /// meant whatever the extension held when asked: a pin request from an earlier dictation,
    /// arriving late, moved it to another field, and this one's words were delivered there
    /// (Codex's ninth review of the 0.1.8 candidate).
    token: u64,
}

/// A name for the next pin: later ones are larger, so the extension can tell a request that
/// arrives late from the pin that replaced it, and none repeats across restarts of the program
/// (a page can outlive it).
pub fn next_pin_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64);
    // First use: start from the clock. After that, always one more than the last.
    let _ = NEXT.compare_exchange(0, started, Ordering::SeqCst, Ordering::SeqCst);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

impl ChromiumDestination {
    /// Tell the extension to let go of this pin - and only this one: it ignores the request if
    /// the field it holds is another pin's by now.
    pub fn release(&self) {
        let _ = self.bridge.request_for("unpin", None, Some(self.token), Duration::from_millis(300));
    }

    /// Ask the extension to pin whatever the user currently has focused in the browser, under
    /// this name.
    pub fn pin(bridge: Bridge, token: u64) -> Result<Self, String> {
        let v = bridge
            .request_for("pin", None, Some(token), Duration::from_millis(1200))
            .map_err(|e| match e {
                BridgeError::NotConnected => "browser-extension-not-connected".to_string(),
                BridgeError::NoResponse => "browser-did-not-answer".to_string(),
                BridgeError::Transport(m) => m,
            })?;

        if !v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
            return Err(v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("no-editable-field-focused")
                .to_string());
        }

        validate_pin_answer(&v, token)?;

        let t = v.get("target");
        let site = t
            .and_then(|t| t.get("host"))
            .and_then(|h| h.as_str())
            .unwrap_or("a web page");
        let what = t
            .and_then(|t| t.get("describe"))
            .and_then(|d| d.as_str())
            .unwrap_or("text field");

        Ok(ChromiumDestination { bridge, label: format!("{site} — {what}"), token })
    }
}

#[cfg(test)]
mod pin_answer_tests {
    use super::validate_pin_answer;
    use serde_json::json;

    #[test]
    fn accepts_the_owned_pin_protocol_and_matching_name() {
        assert!(validate_pin_answer(&json!({"ok": true, "protocol": "owned-pin-v1", "pin": 200}), 200).is_ok());
    }

    #[test]
    fn refuses_an_old_extension_without_the_capability() {
        assert!(validate_pin_answer(&json!({"ok": true, "target": {"host": "example.test"}}), 200).is_err());
        assert!(validate_pin_answer(&json!({"ok": true, "protocol": "other", "pin": 200}), 200).is_err());
        // The isolated JS harness can pass the actual 0.1.0 script's reply to this same check.
        if let Ok(answer) = std::env::var("HVTT_MOCK_PIN_ANSWER") {
            let answer = serde_json::from_str(&answer).expect("mock page's JSON pin answer");
            assert!(validate_pin_answer(&answer, 200).is_err());
        }
    }

    #[test]
    fn refuses_an_answer_for_a_different_or_missing_name() {
        assert!(validate_pin_answer(&json!({"ok": true, "protocol": "owned-pin-v1", "pin": 100}), 200).is_err());
        assert!(validate_pin_answer(&json!({"ok": true, "protocol": "owned-pin-v1"}), 200).is_err());
    }
}

impl Destination for ChromiumDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        match self.bridge.request_for("status", None, Some(self.token), STATUS_TIMEOUT) {
            // Nothing answered: the tab, page or browser is gone. Refuse.
            Err(_) => Liveness::dead("no-response"),
            Ok(v) => {
                if v.get("alive").and_then(|b| b.as_bool()).unwrap_or(false) {
                    Liveness::Alive
                } else {
                    Liveness::dead(
                        v.get("why").and_then(|w| w.as_str()).unwrap_or("unknown"),
                    )
                }
            }
        }
    }

    // The extension writes the text itself; the clipboard copy plays no part.
    fn deliver(&self, text: &str, _copied: bool) -> Result<(), DeliveryError> {
        let v = self
            .bridge
            .request_for("deliver", Some(text.to_string()), Some(self.token), DELIVER_TIMEOUT)
            .map_err(|_| DeliveryError::NoResponse)?;

        if v.get("delivered").and_then(|b| b.as_bool()).unwrap_or(false) {
            return Ok(());
        }
        // The extension verifies by read-back, so a false here means the write did not land.
        Err(match v.get("refused").and_then(|r| r.as_str()) {
            Some("element-detached") | Some("not-in-document") | Some("page-navigated") => {
                DeliveryError::DestinationLost
            }
            _ => DeliveryError::NotVerified,
        })
    }
}
