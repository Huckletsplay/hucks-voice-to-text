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

/// Huck's own clipboard: a named macOS pasteboard.
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
