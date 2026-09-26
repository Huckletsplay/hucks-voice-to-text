//! Windows UI Automation destination - the twin of `macos_ax`, with the same rules:
//!
//! - **The held `IUIAutomationElement` is the identity.** Never re-found from attributes; a
//!   look-alike is indistinguishable and must never be written to.
//! - **Liveness gates delivery** (enforced in `hvtt_core::pipeline`, checked here).
//! - **Writes are verified by reading back.** A return code proves nothing.
//! - **Password boxes are refused** (`IsPassword`), checked before any write.
//! - **No fallback steals the foreground.** When both silent writes fail, the only further rung
//!   is `windows_paste`, and only while nothing has moved since the keypress.
//!
//! Two silent writes, politest first:
//!
//! 1. **`EM_REPLACESEL`** into a classic Win32 edit or rich-edit control (Notepad, WordPad, most
//!    dialog boxes): inserts at his caret, exactly like `AXSelectedText`, and keeps working after
//!    he clicks away because the control keeps its own selection.
//! 2. **`ValuePattern.SetValue`**, the field's text plus the words (WPF and WinUI text boxes) -
//!    the twin of setting `AXValue`.
//!
//! UI Automation needs no permission grant on Windows, so there is no Accessibility step.
//!
//! COM: every entry point joins the multithreaded apartment. The app only calls in from its own
//! worker threads, never from the window thread.

use hvtt_core::pipeline::{DeliveryError, Destination, Liveness};
use windows::core::{Interface, BSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_ValuePatternId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_GETTEXT, WM_GETTEXTLENGTH,
};

use super::windows_paste::{class_of, FocusStamp, PasteDestination};

const EM_REPLACESEL: u32 = 0x00C2;
/// A hung target must never hang delivery; the words are on the clipboard already.
const SEND_TIMEOUT_MS: u32 = 1000;

fn automation() -> Result<IUIAutomation, String> {
    unsafe {
        // S_FALSE (already joined) is fine. The app's worker threads start with no apartment.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("ui-automation-unavailable:{e}"))
    }
}

/// The executable behind a process id, as a full path. Empty when Windows will not say.
pub fn executable_of(pid: u32) -> String {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        if ok.is_err() {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// "notepad" from `C:\Windows\System32\notepad.exe`, as a name to show him: "Notepad".
pub fn app_name(exe: &str) -> String {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file);
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "that app".into(),
    }
}

/// A focused element, captured right after the keypress.
pub struct UiaElement {
    element: IUIAutomationElement,
}

// UI Automation client objects are free-threaded; the app also keeps this behind a lock.
unsafe impl Send for UiaElement {}
unsafe impl Sync for UiaElement {}

/// What has keyboard focus - provided nothing has moved since the keypress `stamp` was taken.
///
/// UI Automation asks the other app, so it runs on a worker thread a few milliseconds after the
/// keypress rather than on it. The stamp closes that gap: if the foreground, the focused control
/// or his hands moved in between, the capture is refused rather than taking the new field.
pub fn capture(stamp: &FocusStamp) -> Result<UiaElement, String> {
    let uia = automation()?;
    let element = unsafe { uia.GetFocusedElement() }.map_err(|_| "nothing-focused".to_string())?;
    if let Some(why) = stamp.moved() {
        return Err(format!("moved-before-capture:{why}"));
    }
    if unsafe { element.CurrentProcessId() }.ok() == Some(std::process::id() as i32) {
        return Err("own-window".into());
    }
    Ok(UiaElement { element })
}

/// Turn a captured element into a destination.
pub fn validate_captured(captured: UiaElement, app: String) -> Result<UiaDestination, String> {
    let element = captured.element;
    unsafe {
        // Never dictate into a password box. Checked first, before anything else.
        if element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) {
            return Err("secure-field".into());
        }
        let role = element.CurrentControlType().map_err(|_| "element-unreachable".to_string())?;
        if role != UIA_EditControlTypeId && role != UIA_DocumentControlTypeId {
            return Err(format!("not-a-text-field:{}", role.0));
        }
        let pid = element.CurrentProcessId().map_err(|_| "element-unreachable".to_string())?;
        let hwnd = element.CurrentNativeWindowHandle().map(|h| h.0 as isize).unwrap_or(0);
        let edit_class = (hwnd != 0).then(|| class_of(hwnd)).filter(|c| is_edit_class(c));
        let detail = element
            .CurrentName()
            .ok()
            .map(|n| n.to_string())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "text field".into());
        Ok(UiaDestination {
            element,
            label: format!("{app} — {detail}"),
            pid,
            role,
            edit_hwnd: edit_class.map(|_| hwnd),
            paste: None,
        })
    }
}

/// Classic Win32 text controls that take `EM_REPLACESEL` - Notepad's is `Edit` on Windows 10 and
/// `RichEditD2DPT` on 11.
fn is_edit_class(class: &str) -> bool {
    class == "Edit" || class.starts_with("RichEdit") || class.starts_with("RICHEDIT")
}

/// A pinned text field.
pub struct UiaDestination {
    element: IUIAutomationElement,
    label: String,
    pid: i32,
    role: UIA_CONTROLTYPE_ID,
    /// Set for classic edit controls, which take a silent insert at the caret.
    edit_hwnd: Option<isize>,
    paste: Option<PasteDestination>,
}

unsafe impl Send for UiaDestination {}
unsafe impl Sync for UiaDestination {}

impl UiaDestination {
    /// The last rung, for fields that accept neither silent write.
    pub fn with_paste_fallback(mut self, paste: Option<PasteDestination>) -> Self {
        self.paste = paste;
        self
    }

    fn value_pattern(&self) -> Option<IUIAutomationValuePattern> {
        unsafe { self.element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok() }
    }

    /// The field's whole text, through whichever door it opens.
    fn value(&self) -> Option<String> {
        if let Some(hwnd) = self.edit_hwnd {
            return window_text(hwnd);
        }
        let pattern = self.value_pattern()?;
        unsafe { pattern.CurrentValue().ok().map(|v| v.to_string()) }
    }

    fn replace_selection(&self, hwnd: isize, text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut result = 0usize;
        unsafe {
            // Windows copies the string across to the other process for this message.
            SendMessageTimeoutW(
                HWND(hwnd as *mut _),
                EM_REPLACESEL,
                WPARAM(1), // can be undone, like typing
                LPARAM(wide.as_ptr() as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                SEND_TIMEOUT_MS,
                Some(&mut result),
            )
            .0 != 0
        }
    }

    fn set_value(&self, text: &str) -> bool {
        let Some(pattern) = self.value_pattern() else { return false };
        unsafe {
            if pattern.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true) {
                return false;
            }
            pattern.SetValue(&BSTR::from(text)).is_ok()
        }
    }
}

fn window_text(hwnd: isize) -> Option<String> {
    let h = HWND(hwnd as *mut _);
    unsafe {
        let mut len = 0usize;
        if SendMessageTimeoutW(h, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, SEND_TIMEOUT_MS, Some(&mut len)).0 == 0 {
            return None;
        }
        let mut buf = vec![0u16; len + 1];
        let mut copied = 0usize;
        if SendMessageTimeoutW(
            h,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            SEND_TIMEOUT_MS,
            Some(&mut copied),
        )
        .0 == 0
        {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..copied.min(len)]))
    }
}

impl Destination for UiaDestination {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_alive(&self) -> Liveness {
        let _ = automation();
        unsafe {
            // A dead element answers nothing. Its process and role are the cheapest probes.
            match self.element.CurrentProcessId() {
                Err(_) => return Liveness::dead("element-unreachable"),
                Ok(p) if p != self.pid => return Liveness::dead("process-changed"),
                Ok(_) => {}
            }
            match self.element.CurrentControlType() {
                Ok(r) if r == self.role => {}
                _ => return Liveness::dead("role-changed"),
            }
            if let Some(hwnd) = self.edit_hwnd {
                if !IsWindow(Some(HWND(hwnd as *mut _))).as_bool() {
                    return Liveness::dead("window-closed");
                }
            }
        }
        Liveness::Alive
    }

    fn deliver(&self, text: &str) -> Result<(), DeliveryError> {
        let _ = automation();
        // Re-check immediately before writing: a field can become a password box.
        if unsafe { self.element.CurrentIsPassword() }.map(|b| b.as_bool()).unwrap_or(false) {
            return Err(DeliveryError::RefusedSecureField);
        }

        let before = self.value().unwrap_or_default();

        // Politest first: insert at the caret without rewriting the field.
        if let Some(hwnd) = self.edit_hwnd {
            if self.replace_selection(hwnd, text) && verified(&self.value(), &before, text) {
                return Ok(());
            }
        }

        if self.set_value(&format!("{before}{text}")) && verified(&self.value(), &before, text) {
            return Ok(());
        }

        // Both silent writes failed. A paste delivers to whatever is focused now, so it is only
        // allowed while nothing has moved since the keypress - then that is still this field.
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

// Keeps `Interface` in scope for `GetCurrentPatternAs`.
const _: fn(&IUIAutomationElement) -> *mut std::ffi::c_void = |e| e.as_raw();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names_read_like_names() {
        assert_eq!(app_name(r"C:\Windows\System32\notepad.exe"), "Notepad");
        assert_eq!(app_name(r"C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE"), "WINWORD");
        assert_eq!(app_name(""), "that app");
    }

    #[test]
    fn classic_edit_controls_are_recognised() {
        for class in ["Edit", "RichEdit20W", "RICHEDIT50W", "RichEditD2DPT"] {
            assert!(is_edit_class(class), "{class}");
        }
        assert!(!is_edit_class("Chrome_WidgetWin_1"));
        assert!(!is_edit_class("Scintilla"));
    }

    #[test]
    fn a_write_counts_only_when_it_reads_back() {
        assert!(verified(&Some("hello world".into()), "hello ", "world"));
        assert!(!verified(&Some("hello ".into()), "hello ", "world"), "nothing changed");
        assert!(!verified(&None, "", "world"), "unreadable is unverified");
        assert!(!verified(&Some("x".into()), "", ""), "empty text is never a delivery");
    }
}
