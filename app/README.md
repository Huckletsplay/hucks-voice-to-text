# Huck's Voice to Text

Local, offline dictation. Press a shortcut, speak, press it again — the words go into the text box
you started in, and they are never lost.

Everything runs on the machine. There is no account and no telemetry, and the only network
connection is **Check for Updates**, made to GitHub when the user chooses it.

**Status:** macOS on Apple silicon, public unsigned beta. Releases:
https://github.com/Huckletsplay/hucks-voice-to-text/releases. Windows 10/11 builds from this source
and runs; there is no Windows release yet.

## Requirements

- macOS 11 or later (developed and tested on Apple Silicon, macOS 26), with Rust (stable) and
  CMake
- or Windows 10/11 (x64), with Rust (stable, MSVC) and Visual Studio 2022 Build Tools: the C++
  workload plus the "C++ CMake tools" and "C++ Clang tools" components. The WebView2 runtime that
  ships with Windows is used for the floating box.
- A Whisper model, downloaded once

## Getting started

```sh
scripts/fetch-model.sh base.en   # ~141 MB, into the app-data directory
scripts/install.sh               # build, sign locally, install to ~/Applications, launch
```

`install.sh` builds a release binary, assembles a `.app` (`assemble-app.sh`), signs it with a
**local** code-signing certificate named `Project Playground Local Signing` when one exists (set
`HVTT_SIGN_IDENTITY` for another), so microphone and Accessibility permissions survive rebuilds —
this is not public signing — and installs it to `~/Applications`. It runs as a background app: **no Dock icon**, an H
in the menu bar, and the global shortcut. Settings, models and drafts live in
`~/Library/Application Support/com.huck.voice-to-text` and are untouched by reinstalling.

On first use macOS asks for **Microphone** and **Accessibility** access. Accessibility is what
lets it type into other apps.

For development, use `scripts/dev.sh` rather than calling `cargo` directly — it handles two
environment quirks of this machine and drive. See the comment at the top of the script.

```sh
scripts/dev.sh build
scripts/dev.sh run
scripts/dev.sh test                 # the whole suite
scripts/dev.sh test -- --ignored    # also the two tests that use the real clipboard
scripts/release.sh --unsigned-beta  # the public DMG and its checksum, in ../artifacts/macos/release
```

On **Windows** the same steps are PowerShell scripts:

```powershell
scripts\fetch-model.ps1             # ~141 MB, into %LOCALAPPDATA%\Huck's Voice to Text\models
scripts\install.ps1                 # release build, installed to %LOCALAPPDATA%\Programs\HucksVoiceToText
scripts\dev.ps1 build | run | test  # development; `dev.ps1 test --ignored` adds the on-purpose tests
```

`dev.ps1` finds CMake and libclang in the Build Tools and sets whisper.cpp's MSVC Release flags
(the `cmake` crate otherwise builds it without `/O2`). Its builds are tuned to the building
machine's processor, so they are for that machine only; `scripts\release.ps1 -UnsignedBeta` makes
the public installer instead - AVX2 baseline, static C runtime, build paths removed and checked.
No permission step exists on Windows.

The release DMG carries the speech model inside the app (`Contents/Resources/models/`); a model in
Application Support is preferred when present. Its file names are the contract with the in-app
updater in `desktop/src/update.rs`.

## Using it

| Action | How |
|---|---|
| Start / stop dictation | `Option + Space` (Windows: `Alt + Space`), or H › Start Dictation |
| Where the words go | Copied to the chosen clipboard first, then into the text box that had focus when you pressed the shortcut |
| If the box is gone | The words are already on the clipboard — paste them wherever you want |
| Choose the clipboard | H › Settings › Clipboard: *Normal Clipboard — ⌘V* (default) or *Huck's Clipboard — Ctrl + Option + V* (Windows: Ctrl+V, or Ctrl + Alt + Shift + V). One or the other, never both |
| Change a shortcut | H › Settings › Shortcuts, pick one, press the new keys (Esc cancels) |
| Recovery drafts | H › Settings › Keep Recovery Drafts, and Open Drafts Folder |
| Update | H › Settings › Check for Updates… — downloads a newer DMG from GitHub Releases, checks its SHA-256, then offers to open it |
| Close the floating box | It leaves by itself; the × or `Esc` closes it early |

Every setting lives in the menu-bar H (on Windows, the H in the notification area); there is no
settings window. The floating box never takes the foreground — the app runs as a macOS accessory,
and on Windows the box is a non-activating window — except, on macOS, while waiting for a new
shortcut, which needs the keyboard. On Windows a keyboard hook hears the new shortcut instead, so
chords Windows keeps for itself, such as Alt + Space, can be chosen.

## Where dictation can go

| Destination | How | After clicking away |
|---|---|---|
| Native macOS apps | Accessibility API, silent write, verified by reading back | still delivered |
| Safari | same | still delivered |
| Chrome and other Chromium browsers | the extension (`extension/`), over Native Messaging | still delivered, once the extension and host are installed |
| Electron apps (VS Code, Slack, Discord), Chrome without the extension | a Cmd+V, only if nothing moved since the keypress — same app and window, no click, no typing | not delivered; the words wait on the clipboard |
| Windows: classic and modern Windows apps (Notepad, WordPad, WPF, WinUI) | UI Automation: `EM_REPLACESEL` at the caret, else `ValuePattern`; verified by reading back | still delivered |
| Windows: Electron apps, Chrome and Edge | a Ctrl+V under the same "nothing moved" rule, sent at once even with the shortcut still held | not delivered; the words wait on the clipboard |

A field that has closed or been replaced is never written to, and nothing tries to find it again.
Nothing is ever submitted: the program never presses Enter.

## How it is put together

```
core/         hvtt-core - platform-free and fully tested. Session state machine, transcript
              model, completion pipeline (the safety gate), pinning, settings, audio, engine seam.
desktop/      The Tauri application.
  src/        microphone capture (cpal), Whisper, clipboards (clip.rs), the browser bridge, the
              menu-bar menu (lib.rs), Check for Updates (update.rs).
    destination/  the only platform-specific delivery code: macOS Accessibility and Windows UI
                  Automation, each platform's gated paste, and Chromium.
    win_*.rs      Windows only: the non-activating floating box and the shortcut prompt.
  ui/         the floating box: plain HTML, CSS and JavaScript. No bundler, no npm dependencies.
  tests/      end-to-end tests, including the on-purpose real-clipboard tests.
extension/    the Chromium extension. Separate on purpose, so a store build is packaging only.
scripts/      dev.sh, install.sh, assemble-app.sh, release.sh, fetch-model.sh,
              install-native-host.sh (macOS); dev.ps1, install.ps1, fetch-model.ps1, release.ps1,
              HucksVoiceToText.iss - the Inno Setup installer (Windows);
              icon generators.
```

`core` contains no platform code and no I/O beyond the filesystem, so its tests run anywhere and
the Windows version reuses it unchanged. Everything OS-native sits behind
`hvtt_core::pipeline::Destination` and the `Clipboard` trait.

### The one rule

**A finished transcript is never lost.** `complete_transcription` writes a recovery draft, then
copies to the clipboard, then attempts delivery — in that order, always. The tests in
`core/src/pipeline.rs` exist to keep it that way.

## Where things live

Never inside this repository:

| What | Where |
|---|---|
| Models | `~/Library/Application Support/com.huck.voice-to-text/models/` |
| Settings | `~/Library/Application Support/com.huck.voice-to-text/settings.json` |
| Recovery drafts | `~/Library/Application Support/com.huck.voice-to-text/drafts/` |
| Huck's clipboard | the named macOS pasteboard `com.huck.clipboard` |
| Build output | `~/.hvtt-build/target` |
| Windows: models, settings, drafts | `%LOCALAPPDATA%\Huck's Voice to Text\` (`models\`, `settings.json`, `drafts\`) |
| Windows: Huck's clipboard | the running program's memory — Windows has no named clipboards |
| Windows: build output | `%LOCALAPPDATA%\hvtt-build\target` |

## Third-party dependencies

| Crate | Licence | Why |
|---|---|---|
| `tauri` + plugins | MIT / Apache-2.0 | Window, menu-bar item, global shortcuts, clipboard |
| `whisper-rs` | Unlicense | Bindings to whisper.cpp |
| `whisper.cpp` (vendored by the above) | MIT | Local speech recognition |
| `cpal` | Apache-2.0 | Microphone capture |
| `hound` | Apache-2.0 | WAV reading, tests only |
| `core-foundation` | MIT / Apache-2.0 | macOS Accessibility and Core Foundation types |
| `objc2`, `objc2-foundation` | MIT | macOS pasteboards |
| `objc2-app-kit` | Zlib / Apache-2.0 / MIT | macOS pasteboards |
| `windows` | MIT / Apache-2.0 | Windows UI Automation, keyboard input, clipboard, window styles |
| `uds_windows` | MIT | Unix domain sockets on Windows, for the browser bridge |
| `serde`, `serde_json` | MIT / Apache-2.0 | Settings |
| `parking_lot` | MIT / Apache-2.0 | Locks |
| `dirs` | MIT / Apache-2.0 | OS app-data paths |
| `thiserror` | MIT / Apache-2.0 | Error types |

Whisper model weights are MIT, converted by the whisper.cpp project; the release DMG includes
`ggml-base.en.bin`. Updates are fetched with the system's own `curl` and checked with `shasum`
(Windows: `curl.exe` and `certutil`), so there is no HTTP or crypto crate. No code was copied from any third-party application.

## Licence

GNU General Public License version 3 — see `../LICENSE` and `../COPYRIGHT`.
