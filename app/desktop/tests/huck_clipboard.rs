//! Huck's own clipboard, against the real macOS pasteboards.
//!
//! `#[ignore]`d because the borrow test briefly puts text on the user's actual clipboard. Run on
//! purpose: `scripts/dev.sh test -- --ignored`.

#![cfg(target_os = "macos")]

use hvtt_desktop::clip::huck;

#[test]
#[ignore]
fn huck_clipboard_holds_its_own_text() {
    // His last dictation is on there; the test must leave it exactly as it found it.
    let before = huck::read();
    huck::write("held on Huck's clipboard").unwrap();
    let held = huck::read();
    match before {
        Some(text) => huck::write(&text).unwrap(),
        None => huck::clear(),
    }
    assert_eq!(held.as_deref(), Some("held on Huck's clipboard"));
}

#[test]
#[ignore]
fn borrowing_the_normal_clipboard_puts_back_exactly_what_was_there() {
    // Whatever he has copied - text, an image, a file - must come back byte for byte.
    let before = huck::snapshot_general();
    let borrowed = huck::borrow_general("borrowed for a paste").expect("clipboard refused");
    assert_eq!(huck::general_text().as_deref(), Some("borrowed for a paste"));
    borrowed.give_back();
    assert_eq!(huck::snapshot_general(), before, "the normal clipboard was not restored");
}
