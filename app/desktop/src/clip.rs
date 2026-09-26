//! The system clipboard, behind core's trait.
//!
//! This is the safety net the whole product rests on: if delivery fails, the words are still
//! one paste away.

use hvtt_core::pipeline::Clipboard;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

pub struct SystemClipboard {
    app: AppHandle,
}

impl SystemClipboard {
    pub fn new(app: AppHandle) -> Self {
        SystemClipboard { app }
    }
}

impl Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) -> Result<(), String> {
        self.app
            .clipboard()
            .write_text(text.to_string())
            .map_err(|e| e.to_string())
    }
}

/// Huck's own clipboard: a named macOS pasteboard. (Windows: see the second `huck` below.)
///
/// Not a new program or a background task - a private, named slot in the clipboard service the
/// Mac already runs. Written once per dictation, read when he presses the paste shortcut, and
/// idle otherwise. The name is shared on purpose, so Huck's other programs can use the same one.
#[cfg(target_os = "macos")]
pub mod huck {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    pub const NAME: &str = "com.huck.clipboard";

    fn board() -> Retained<NSPasteboard> {
        NSPasteboard::pasteboardWithName(&NSString::from_str(NAME))
    }

    fn put(board: &NSPasteboard, text: &str) -> bool {
        board.clearContents();
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
    }

    pub fn write(text: &str) -> Result<(), String> {
        put(&board(), text).then_some(()).ok_or_else(|| "Huck's clipboard refused the text".into())
    }

    pub fn clear() {
        board().clearContents();
    }

    pub fn read() -> Option<String> {
        board().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
    }

    /// The pipeline's safety-net copy, sent to Huck's clipboard instead of the normal one.
    pub struct HuckClipboard;

    impl hvtt_core::pipeline::Clipboard for HuckClipboard {
        fn set_text(&self, text: &str) -> Result<(), String> {
            write(text)
        }
    }

    /// Everything on the normal clipboard, every item and every type, so an image or a copied
    /// file comes back exactly as it was - not just its text.
    pub struct Borrowed {
        items: Vec<Vec<(String, Vec<u8>)>>,
        ours: isize,
    }

    /// Every item and type on the normal clipboard, as plain bytes.
    pub fn snapshot_general() -> Vec<Vec<(String, Vec<u8>)>> {
        NSPasteboard::generalPasteboard()
            .pasteboardItems()
            .map(|list| {
                list.iter()
                    .map(|item| {
                        item.types()
                            .iter()
                            .filter_map(|t| {
                                let data = item.dataForType(&t)?;
                                Some((t.to_string(), data.to_vec()))
                            })
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Put `text` on the normal clipboard for a moment, remembering what was there. A paste into
    /// an app like VS Code can only come from the normal clipboard.
    pub fn borrow_general(text: &str) -> Option<Borrowed> {
        let items = snapshot_general();
        let general = NSPasteboard::generalPasteboard();
        put(&general, text).then(|| Borrowed { items, ours: general.changeCount() })
    }

    /// What the normal clipboard holds as text, if anything.
    pub fn general_text() -> Option<String> {
        NSPasteboard::generalPasteboard()
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|s| s.to_string())
    }

    impl Borrowed {
        /// Put his clipboard back - unless he copied something new in the meantime, which wins.
        pub fn give_back(self) {
            let general = NSPasteboard::generalPasteboard();
            if general.changeCount() != self.ours {
                return;
            }
            general.clearContents();
            if self.items.is_empty() {
                return;
            }
            let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = self
                .items
                .iter()
                .map(|types| {
                    let item = NSPasteboardItem::new();
                    for (t, bytes) in types {
                        item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(t));
                    }
                    ProtocolObject::from_retained(item)
                })
                .collect();
            general.writeObjects(&NSArray::from_retained_slice(&items));
        }
    }
}

/// Huck's own clipboard on Windows.
///
/// Windows has no named clipboards, so this one lives in the running program's memory - which is
/// always there, because the program stays resident for the hotkey. Deliberately not a file: with
/// Keep Recovery Drafts off he has asked for no dictation to touch the disk. The same functions as
/// the macOS module, so the rest of the app does not care which it is talking to.
#[cfg(windows)]
pub mod huck {
    use parking_lot::Mutex;
    use windows::core::w;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

    static HELD: Mutex<Option<String>> = Mutex::new(None);

    const CF_UNICODETEXT: u32 = 13;

    pub fn write(text: &str) -> Result<(), String> {
        *HELD.lock() = Some(text.to_string());
        Ok(())
    }

    pub fn clear() {
        *HELD.lock() = None;
    }

    pub fn read() -> Option<String> {
        HELD.lock().clone()
    }

    /// The pipeline's safety-net copy, sent to Huck's clipboard instead of the normal one.
    pub struct HuckClipboard;

    impl hvtt_core::pipeline::Clipboard for HuckClipboard {
        fn set_text(&self, text: &str) -> Result<(), String> {
            write(text)
        }
    }

    /// The Windows clipboard, open for as long as this lives. Another program may hold it for a
    /// moment, so opening retries briefly rather than failing the paste.
    struct Open;

    impl Open {
        fn new() -> Option<Open> {
            for _ in 0..20 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Some(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// Formats whose data is a GDI handle rather than memory. Windows rebuilds the common ones
    /// (a bitmap from its DIB) by itself, so they are left out rather than copied wrongly.
    fn is_gdi(format: u32) -> bool {
        matches!(format, 2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x300..=0x3FF)
    }

    fn global_from(bytes: &[u8]) -> Option<HGLOBAL> {
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
            let target = GlobalLock(memory) as *mut u8;
            if target.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            let _ = GlobalUnlock(memory);
            Some(memory)
        }
    }

    fn put(format: u32, bytes: &[u8]) -> bool {
        match global_from(bytes) {
            // Once set, the memory belongs to the clipboard.
            Some(memory) => unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) }.is_ok(),
            None => false,
        }
    }

    fn utf16z(text: &str) -> Vec<u8> {
        text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
    }

    /// Everything on the normal clipboard, every format, as plain bytes - so an image or a copied
    /// file comes back exactly as it was, not just its text.
    pub fn snapshot_general() -> Vec<(u32, Vec<u8>)> {
        let Some(_open) = Open::new() else { return Vec::new() };
        let mut items = Vec::new();
        let mut format = 0u32;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if is_gdi(format) {
                continue;
            }
            unsafe {
                let Ok(handle) = GetClipboardData(format) else { continue };
                let memory = HGLOBAL(handle.0);
                let size = GlobalSize(memory);
                let source = GlobalLock(memory) as *const u8;
                if source.is_null() {
                    continue;
                }
                items.push((format, std::slice::from_raw_parts(source, size).to_vec()));
                let _ = GlobalUnlock(memory);
            }
        }
        items
    }

    /// What the normal clipboard holds as text, if anything.
    pub fn general_text() -> Option<String> {
        let _open = Open::new()?;
        unsafe {
            let memory = HGLOBAL(GetClipboardData(CF_UNICODETEXT).ok()?.0);
            let source = GlobalLock(memory) as *const u16;
            if source.is_null() {
                return None;
            }
            let units = GlobalSize(memory) / 2;
            let slice = std::slice::from_raw_parts(source, units);
            let end = slice.iter().position(|&u| u == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&slice[..end]);
            let _ = GlobalUnlock(memory);
            Some(text)
        }
    }

    /// Everything that was on the normal clipboard before a borrow.
    pub struct Borrowed {
        items: Vec<(u32, Vec<u8>)>,
        ours: u32,
    }

    /// Put `text` on the normal clipboard for a moment, remembering what was there. A paste into
    /// an app like VS Code can only come from the normal clipboard.
    ///
    /// Marked so Windows' clipboard history (Win+V) does not keep the borrowed copy - it is his
    /// words on loan for half a second, not something he copied.
    pub fn borrow_general(text: &str) -> Option<Borrowed> {
        let items = snapshot_general();
        {
            let _open = Open::new()?;
            unsafe {
                EmptyClipboard().ok()?;
                if !put(CF_UNICODETEXT, &utf16z(text)) {
                    return None;
                }
                let private = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
                if private != 0 {
                    let _ = put(private, &[0]);
                }
            }
        }
        // Read only once the clipboard is closed: closing it is itself a change Windows counts.
        Some(Borrowed { items, ours: unsafe { GetClipboardSequenceNumber() } })
    }

    impl Borrowed {
        /// Put his clipboard back - unless he copied something new in the meantime, which wins.
        pub fn give_back(self) {
            if unsafe { GetClipboardSequenceNumber() } != self.ours {
                return;
            }
            let Some(_open) = Open::new() else { return };
            unsafe {
                let _ = EmptyClipboard();
            }
            for (format, bytes) in &self.items {
                put(*format, bytes);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn gdi_handle_formats_are_not_copied_as_memory() {
            for format in [2, 3, 9, 14, 0x80, 0x300, 0x3FF] {
                assert!(is_gdi(format), "{format:#x}");
            }
            for format in [1, 8, 13, 15, 17, 0xC0FF] {
                assert!(!is_gdi(format), "{format:#x} is plain memory");
            }
        }

        #[test]
        fn text_goes_on_as_nul_terminated_utf16() {
            assert_eq!(utf16z("Hi"), vec![b'H', 0, b'i', 0, 0, 0]);
        }
    }
}
