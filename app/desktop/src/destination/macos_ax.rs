//! macOS Accessibility destination — production implementation of the spike's findings.
//!
//! Rules, all of them learned the hard way and none of them optional:
//!
//! - **The held `AXUIElementRef` is the identity.** Never re-found from attributes: after a page
//!   reload the replacement field matched the original on all 18 measured attributes, so a
//!   look-alike is indistinguishable and must never be written to.
//! - **Liveness gates delivery** (enforced in `hvtt_core::pipeline`, checked here).
//! - **Writes are verified by reading back.** `AXSelectedText` returns success on WebKit while
//!   inserting nothing, so a return code proves nothing.
//! - **Secure fields are refused by subrole**, before any write. Password prompts advertise
//!   themselves as writable.
//! - **No fallback steals the foreground.** When both silent writes fail, the only further rung
//!   is `macos_paste`, and only while nothing has moved since the keypress - so the field being
//!   pasted into is still the pinned one.

#![allow(non_upper_case_globals)]

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use hvtt_core::pipeline::{DeliveryError, Destination, Liveness};
use std::ffi::c_void;

pub type AXUIElementRef = CFTypeRef;
type AXError = i32;

const kAXErrorSuccess: AXError = 0;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> AXError;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> AXError;
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

/// Is the Accessibility permission granted to this process?
///
/// It is a manual, per-machine grant attached to the running binary. It cannot be automated or
/// bundled, and without it pinned delivery silently does nothing — which is why the UI asks for
/// it at the moment the feature is first used rather than at install.
pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS to put this app in the Accessibility list, with its own one-time prompt.
///
/// The plain check above never adds the app to System Settings, so the pane opened with nothing
/// to switch on and the grant meant hunting for the app with the + button. This registers it.
pub fn request_accessibility() -> bool {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as CFTypeRef)
    }
}

/// An owned AX element reference.
///
/// `AXUIElementRef` is a CF type, so it is retained on capture and released on drop. Holding it
/// is the pin; there is deliberately no way to reconstruct one.
pub struct AxElement(AXUIElementRef);

// AX element references may be used from any thread; the app also keeps this behind a lock.
unsafe impl Send for AxElement {}
unsafe impl Sync for AxElement {}

impl AxElement {
    unsafe fn retain(raw: AXUIElementRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        CFRetain(raw);
        Some(AxElement(raw))
    }

    fn copy_string(&self, attribute: &str) -> Option<String> {
        unsafe {
            let key = CFString::new(attribute);
            let mut value: CFTypeRef = std::ptr::null();
            if AXUIElementCopyAttributeValue(self.0, key.as_concrete_TypeRef(), &mut value)
                != kAXErrorSuccess
                || value.is_null()
            {
                return None;
            }
            let s = CFString::wrap_under_create_rule(value as CFStringRef).to_string();
            Some(s)
        }
    }

    fn copy_element(&self, attribute: &str) -> Option<AxElement> {
        unsafe {
            let key = CFString::new(attribute);
            let mut value: CFTypeRef = std::ptr::null();
            if AXUIElementCopyAttributeValue(self.0, key.as_concrete_TypeRef(), &mut value)
                != kAXErrorSuccess
                || value.is_null()
            {
                return None;
            }
            // wrap_under_create_rule semantics: we own it, so hand ownership to AxElement.
            Some(AxElement(value))
        }
    }

    fn is_settable(&self, attribute: &str) -> bool {
        unsafe {
            let key = CFString::new(attribute);
            let mut settable: u8 = 0;
            AXUIElementIsAttributeSettable(self.0, key.as_concrete_TypeRef(), &mut settable)
                == kAXErrorSuccess
                && settable != 0
        }
    }

    fn set_string(&self, attribute: &str, text: &str) -> bool {
        unsafe {
            let key = CFString::new(attribute);
            let val = CFString::new(text);
            AXUIElementSetAttributeValue(
                self.0,
                key.as_concrete_TypeRef(),
                val.as_concrete_TypeRef() as CFTypeRef,
            ) == kAXErrorSuccess
        }
    }

    fn pid(&self) -> Option<i32> {
        unsafe {
            let mut pid: i32 = 0;
            (AXUIElementGetPid(self.0, &mut pid) == kAXErrorSuccess).then_some(pid)
        }
    }
}

/// Whether a captured field is a password box.
///
/// `None` is deliberately different from `Some(false)`: Accessibility failing to answer must
/// never be treated as permission to paste or write. Secure fields advertise themselves as
/// writable, so this check is the only reliable guard.
pub fn captured_is_password(element: &AxElement) -> Option<bool> {
    password_state(element.copy_string("AXSubrole").as_deref())
}

fn password_state(subrole: Option<&str>) -> Option<bool> {
    subrole.map(|value| value == "AXSecureTextField")
}

impl Clone for AxElement {
    fn clone(&self) -> Self {
        unsafe {
            CFRetain(self.0);
        }
        AxElement(self.0)
    }
}

impl std::fmt::Debug for AxElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AxElement({:p})", self.0)
    }
}

impl Drop for AxElement {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                CFRelease(self.0);
            }
        }
    }
}

/// The focus source used at the instant the shortcut arrives.
///
/// Everything here is the plain Accessibility C API, which costs microseconds. Nothing on this
/// path shells out: reading the frontmost application through `osascript` takes about 100 ms,
/// and in 100 ms the user has already clicked somewhere else — which is the entire race this
/// exists to remove.
pub struct AxFocusSource;

impl hvtt_core::pinning::FocusSource for AxFocusSource {
    type Handle = AxElement;

    fn focused_now(&self) -> Option<AxElement> {
        if !accessibility_trusted() {
            return None;
        }
        unsafe {
            let system = AxElement::retain(AXUIElementCreateSystemWide())?;
            // The system-wide element alone is unreliable - it returns nothing while a field
            // plainly has focus - so the focused *application* is the documented fallback.
            // Both are pure C calls.
            system.copy_element("AXFocusedUIElement").or_else(|| {
                let app = system.copy_element("AXFocusedApplication")?;
                app.copy_element("AXFocusedUIElement")
                    .or_else(|| app.copy_element("AXFocusedWindow")?.copy_element("AXFocusedUIElement"))
            })
        }
    }
}

/// Turn a captured element into a destination. Slow enough to belong off the keypress path.
pub fn validate_captured(element: AxElement) -> Result<AxDestination, String> {
    AxDestination::from_element(element)
}

/// The executable behind a pid, used to tell a Chromium browser from anything else.
pub fn executable_of(pid: i32) -> String {
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The pid owning a captured element, so classification never needs to re-read focus.
pub fn pid_of(element: &AxElement) -> Option<i32> {
    element.pid()
}

/// Capture whatever currently has keyboard focus.
///
/// The system-wide element alone is unreliable — it returns nothing while a field plainly has
/// focus — so the frontmost application's own element is tried next. The spike's every capture
/// answered by that second route.
pub fn capture_focused() -> Result<AxDestination, String> {
    if !accessibility_trusted() {
        return Err("accessibility-permission-missing".into());
    }
    unsafe {
        let system = AxElement::retain(AXUIElementCreateSystemWide())
            .ok_or("could not reach the accessibility system")?;

        let focused = system
            .copy_element("AXFocusedUIElement")
            .or_else(|| {
                let pid = frontmost_pid()?;
                let app = AxElement::retain(AXUIElementCreateApplication(pid))?;
                app.copy_element("AXFocusedUIElement")
                    .or_else(|| app.copy_element("AXFocusedWindow")?.copy_element("AXFocusedUIElement"))
            })
            .ok_or("nothing has keyboard focus")?;

        AxDestination::from_element(focused)
    }
}

fn frontmost_pid() -> Option<i32> {
    // Cheap and dependency-free: ask the window server via the running application list.
    // `NSWorkspace.frontmostApplication` needs a run loop to stay fresh, which a Tauri app has.
    use std::process::Command;
    let out = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get unix id of first process whose frontmost is true",
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A pinned native text field.
pub struct AxDestination {
    element: AxElement,
    label: String,
    pid: i32,
    role: String,
    paste: Option<crate::destination::macos_paste::PasteDestination>,
}

impl AxDestination {
    fn from_element(element: AxElement) -> Result<Self, String> {
        let role = element.copy_string("AXRole").unwrap_or_default();

        // Never dictate into a password box. Checked on the subrole, before anything else,
        // because secure fields falsely advertise themselves as writable. Failure to read the
        // subrole is also refusal: "could not tell" is not the same as "not secure".
        match captured_is_password(&element) {
            Some(true) => return Err("secure-field".into()),
            None => return Err("password-unknown".into()),
            Some(false) => {}
        }
        if !matches!(role.as_str(), "AXTextArea" | "AXTextField" | "AXComboBox") {
            return Err(format!("not-a-text-field:{role}"));
        }

        let pid = element.pid().ok_or("no process for the focused element")?;
        let app = app_name(pid);
        let detail = element
            .copy_string("AXDescription")
            .or_else(|| element.copy_string("AXPlaceholderValue"))
            .or_else(|| element.copy_string("AXTitle"))
            .unwrap_or_else(|| "text field".into());

        Ok(AxDestination { element, label: format!("{app} — {detail}"), pid, role, paste: None })
    }

    /// The last rung, for fields that accept neither silent write: Terminal, some web views.
    pub fn with_paste_fallback(
        mut self,
        paste: Option<crate::destination::macos_paste::PasteDestination>,
    ) -> Self {
        self.paste = paste;
        self
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    fn value(&self) -> Option<String> {
        self.element.copy_string("AXValue")
    }
}

fn app_name(pid: i32) -> String {
    use std::process::Command;
    Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            s.rsplit('/').next().map(|n| n.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "application".into())
}

impl Destination for AxDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        // A dead element answers nothing. Reading the role is the cheapest probe.
        match self.element.copy_string("AXRole") {
            None => Liveness::dead("element-unreachable"),
            Some(role) if role != self.role => Liveness::dead("role-changed"),
            Some(_) => {
                // Guard against the pid being recycled by a different process.
                match self.element.pid() {
                    Some(p) if p == self.pid => Liveness::Alive,
                    _ => Liveness::dead("process-changed"),
                }
            }
        }
    }

    fn deliver(&self, text: &str) -> Result<(), DeliveryError> {
        // Re-check the subrole immediately before writing: a field can become secure.
        match captured_is_password(&self.element) {
            Some(true) => return Err(DeliveryError::RefusedSecureField),
            None => return Err(DeliveryError::Other(
                "could not confirm that the field is not secure".into(),
            )),
            Some(false) => {}
        }

        let before = self.value().unwrap_or_default();

        // Politest first: insert at the caret without rewriting the field.
        if self.element.is_settable("AXSelectedText")
            && self.element.set_string("AXSelectedText", text)
        {
            if verified(&self.value(), &before, text) {
                return Ok(());
            }
            // WebKit returns success here and inserts nothing. Fall through rather than lie.
        }

        if self.element.is_settable("AXValue") {
            let combined = format!("{before}{text}");
            if self.element.set_string("AXValue", &combined)
                && verified(&self.value(), &before, text)
            {
                return Ok(());
            }
        }

        // Both silent strategies failed. A paste delivers to whatever is focused now, so it is
        // only allowed while nothing has moved since the keypress - then that is still this field.
        match &self.paste {
            Some(paste) if paste.is_alive().is_alive() => paste.deliver(text),
            _ => Err(DeliveryError::NotVerified),
        }
    }
}

/// A write only counts if it can be read back.
fn verified(after: &Option<String>, before: &str, text: &str) -> bool {
    let Some(after) = after else { return false };
    let norm = |s: &str| s.replace('\u{a0}', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    let (a, b, t) = (norm(after), norm(before), norm(text));
    !t.is_empty() && a != b && a.contains(&t)
}

// Keep the unused-import warning honest about what the FFI needs.
const _: Option<*mut c_void> = None;

#[cfg(test)]
mod tests {
    use super::password_state;

    #[test]
    fn password_subroles_fail_closed() {
        assert_eq!(password_state(Some("AXSecureTextField")), Some(true));
        assert_eq!(password_state(Some("")), Some(false));
        assert_eq!(password_state(Some("AXStandardTextField")), Some(false));
        assert_eq!(password_state(None), None);
    }
}
