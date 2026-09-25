# Huck's Voice to Text — browser extension

Sends dictation into the exact text box you pinned, in Chrome and other Chromium browsers,
without switching windows.

**It exists because the macOS Accessibility API cannot write into Chromium at all** — it reports
the field as writable, returns success, and changes nothing. Measured, repeatedly.

## What it does, and what it refuses to do

- The pin is a **direct reference** to the element you clicked into. Never a selector.
- **No re-finding.** If that element is replaced — even by an identical one — the destination is
  gone. A reloaded page's field matched the original on every attribute we could measure, so a
  look-alike cannot be told apart and must never be written to.
- **Every write is verified** by reading the value back.
- **Silence is refusal.** If the page, tab or browser is gone, nothing answers, and the desktop
  app keeps your text on the clipboard instead of guessing.
- **Password fields are refused**, at pin time and again before writing.
- It never presses Enter, and never brings the browser to the front.

## Permissions

Only `nativeMessaging`, plus a content script. **No `tabs`, no `storage`, no host permissions.**
Routing uses ports opened by each page, so the extension never needs to enumerate your tabs.

The content script currently matches `<all_urls>`, which is the widest possible install prompt.
Before any Web Store submission this should become a narrow opt-in list or `activeTab`.

## Installing during development

Not published. Load it unpacked:

1. `chrome://extensions` → enable **Developer mode**
2. **Load unpacked** → choose this folder
3. Note the extension ID and register the native-messaging host:
   `app/scripts/install-native-host.sh <extension-id>`

`--load-extension` on the command line no longer works in current Chrome; the UI is the only way.

## Structure

| File | Role |
|---|---|
| `content.js` | Owns the pin, the liveness checks and the verified write |
| `background.js` | Transport only — the Native Messaging port, and routing. Holds no state worth losing |
| `manifest.json` | MV3 |

The split is deliberate: MV3 terminates an idle service worker, so anything it held would
vanish mid-dictation. A content script lives exactly as long as its page.
