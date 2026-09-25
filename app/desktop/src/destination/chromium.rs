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

pub struct ChromiumDestination {
    bridge: Bridge,
    label: String,
}

impl ChromiumDestination {
    /// Ask the extension to pin whatever the user currently has focused in the browser.
    pub fn pin(bridge: Bridge) -> Result<Self, String> {
        let v = bridge
            .request("pin", None, Duration::from_millis(1200))
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

        let t = v.get("target");
        let site = t
            .and_then(|t| t.get("host"))
            .and_then(|h| h.as_str())
            .unwrap_or("a web page");
        let what = t
            .and_then(|t| t.get("describe"))
            .and_then(|d| d.as_str())
            .unwrap_or("text field");

        Ok(ChromiumDestination { bridge, label: format!("{site} — {what}") })
    }
}

impl Destination for ChromiumDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        match self.bridge.request("status", None, STATUS_TIMEOUT) {
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

    fn deliver(&self, text: &str) -> Result<(), DeliveryError> {
        let v = self
            .bridge
            .request("deliver", Some(text.to_string()), DELIVER_TIMEOUT)
            .map_err(|_| DeliveryError::NoResponse)?;

        if v.get("delivered").and_then(|b| b.as_bool()).unwrap_or(false) {
            return Ok(());
        }
        // The extension verifies by read-back, so a false here means the write did not land.
        Err(match v.get("refused").and_then(|r| r.as_str()) {
            Some("element-detached") | Some("not-in-document") | Some("page-navigated") => {
                DeliveryError::DestinationLost
            }
            Some("secure-field") => DeliveryError::RefusedSecureField,
            _ => DeliveryError::NotVerified,
        })
    }
}
