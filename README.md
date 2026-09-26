# Huck's Voice to Text

Speak, and your words land in the text box you were typing in.

Huck's Voice to Text is a dictation utility for macOS and Windows. It lives in the menu bar (the
notification area on Windows) instead of taking over your desktop. Press a shortcut, talk, press it again — the words go into the text box you
started in, even if you clicked somewhere else while you were talking.

No account. No cloud. Speech is recognized on your computer.

**Powered by Project Playground**

## Download

Open the [latest release](https://github.com/Huckletsplay/hucks-voice-to-text/releases/latest)
and download:

| Platform | Download | Requirement |
|---|---|---|
| macOS | `HucksVoiceToText-0.1.1-macOS-arm64-unsigned-beta.dmg` | Apple silicon Mac (M1 or newer), macOS 11 or newer |
| Windows | `HucksVoiceToText-0.1.1-windows-x64-setup.exe` | Windows 10 or 11 (64-bit), with a processor that has AVX2 — most PCs from 2015 on |

This is an unsigned public beta: macOS Gatekeeper and Windows SmartScreen will both call the
developer unknown. Installation instructions are below, and each download has a SHA-256 checksum
beside it. The Mac beta has been tested on macOS Tahoe 26.1 on Apple silicon, the Windows beta on
Windows 10 22H2; other versions have not yet been tested. Intel Macs are not supported yet.

## Why use it?

- **Click away without losing your place.** The text box you were in when you pressed the shortcut
  is remembered. Go and check something while you talk; the words still go back where they belong.
- **Never lose what you said.** Before anything is typed, a recovery copy is saved and the words
  are put on the clipboard. If the text box has gone, they are one paste away.
- **Stay out of the way.** No window to manage and no Dock or taskbar button — just an H in the
  menu bar or notification area and a small floating panel while you speak, which says **Sent** or
  **Copied** and disappears.
- **Keep it private.** Recognition runs entirely on your computer with
  [whisper.cpp](https://github.com/ggml-org/whisper.cpp). Audio and text never leave it.
- **Keep your clipboard yours.** Choose the normal clipboard, or Huck's Clipboard, which leaves
  whatever you copied alone and pastes dictation with its own shortcut.
- **Stay safe.** It never types into password fields, never writes into a text box that has closed
  or been replaced, and never presses Enter for you.

## Install on macOS

1. Download and open the DMG.
2. Drag **Huck's Voice to Text** into Applications.
3. Try to open it once. macOS will block this unsigned beta.
4. Open **System Settings > Privacy & Security**, scroll to Security, and choose **Open Anyway**.
5. Open the app again. Look for the **H** in the menu bar.
6. Click into any text box and press **Option + Space**. Allow **Microphone** access when asked.
7. When asked for **Accessibility**, choose **Open System Settings** and switch on
   **Huck's Voice to Text**. This is what lets it type into other apps.

The speech model is included, so there is nothing else to download.

**Updates.** Choose **Settings > Check for Updates…** in the menu-bar H. The app asks GitHub only
when you choose it, downloads a newer DMG with its published SHA-256 checksum, verifies them, and
asks before opening the DMG. Install it the usual way: quit the app, drag the new copy onto
Applications and replace the old one, then reopen it. Because this is still an unsigned beta, macOS
may ask for **Open Anyway** again, and **Microphone** and **Accessibility** need switching on again
for the new copy. Your settings and shortcuts are kept.

## Install on Windows

1. Download `HucksVoiceToText-0.1.1-windows-x64-setup.exe` and run it.
2. Because this beta is not code-signed, Windows may show **Windows protected your PC**. Choose
   **More info**, then **Run anyway**.
3. Follow the installer. It needs no administrator rights: it installs for your account only, into
   `%LOCALAPPDATA%\Programs\HucksVoiceToText`, with a Start menu entry.
4. Look for the **H** in the notification area. Windows may tuck it behind the **^** arrow; drag it
   onto the taskbar to keep it in sight.
5. Click into any text box and press **Alt + Space**. If nothing is heard, check **Settings >
   Privacy > Microphone** and allow desktop apps to use the microphone.

The speech model is included, and Windows needs no Accessibility permission. While the program runs,
**Alt + Space** starts dictation instead of opening a window's system menu; choose another shortcut
under **Settings > Shortcuts** in the H if you prefer.

**Updates.** **Settings > Check for Updates…** in the H downloads a newer installer with its
published SHA-256 checksum and verifies both. **Open Update** installs it and starts the new copy.

**Uninstall** from **Settings > Apps**. Your settings, speech model and recovery drafts stay in
`%LOCALAPPDATA%\Huck's Voice to Text`; delete that folder to remove them too.

## Using it

| To | Do this |
|---|---|
| Start and stop dictation | **Option + Space** (Windows: **Alt + Space**), or **Start Dictation** in the H |
| Change a shortcut | **Settings > Shortcuts**, pick one, and press the new keys |
| Choose the clipboard | **Settings > Clipboard**: **Normal Clipboard** (⌘V; Windows: Ctrl + V) or **Huck's Clipboard** (Control + Option + V; Windows: Ctrl + Alt + Shift + V) |
| Find a lost dictation | **Settings > Open Drafts Folder** — the last 20 are kept, and **Keep Recovery Drafts** turns this off |

Where your words can go:

| You were typing in | What happens | If you clicked away |
|---|---|---|
| Mac apps and Safari | Typed straight in | Still typed into the original box |
| Windows apps such as Notepad and WordPad | Typed straight in | Still typed into the original box |
| VS Code, Slack, Discord, Chrome, Edge and similar apps | Pasted in | Waiting on the clipboard — paste it yourself |

A Chrome extension that keeps the original text box even after you click away is part of the
source (`app/extension/`) but is not yet published in the Chrome Web Store.

## Privacy

Huck's Voice to Text does not require an account, upload audio or text, or collect analytics.
Speech is recognized on your computer, and recovery copies of your dictations stay in
`~/Library/Application Support/com.huck.voice-to-text/drafts/` on a Mac and
`%LOCALAPPDATA%\Huck's Voice to Text\drafts` on Windows. It contacts GitHub only when you
explicitly choose **Check for Updates**. To report a security issue privately, see
[SECURITY.md](SECURITY.md).

## Verify a download

Each download has a checksum file beside it on the release page.

```bash
shasum -a 256 HucksVoiceToText-0.1.1-macOS-arm64-unsigned-beta.dmg
```

```powershell
Get-FileHash .\HucksVoiceToText-0.1.1-windows-x64-setup.exe -Algorithm SHA256
```

Expected SHA-256:

```text
73479b1a0fb7bbe5dd356ce068e2e4be81d0a8ff732876802bb2a4bb81ef110a  HucksVoiceToText-0.1.1-macOS-arm64-unsigned-beta.dmg
29c1e7167d5fdfcd6e20076c38251e1ac8d6200c6127b0cc53846cd389e435f0  HucksVoiceToText-0.1.1-windows-x64-setup.exe
```

## Support

Found a bug or have an idea? Open a
[GitHub issue](https://github.com/Huckletsplay/hucks-voice-to-text/issues) with your macOS or Windows version,
the app you were dictating into, what you expected, and what happened. Please do not paste private
dictated text.

## Source repository

Huck's Voice to Text is written in Rust with [Tauri 2](https://tauri.app), a plain HTML, CSS and
JavaScript floating panel with no npm dependencies, and whisper.cpp for recognition.

```text
app/core/       Platform-free core: the completion pipeline, settings, transcript, audio (fully tested)
app/desktop/    The desktop application for macOS and Windows: recognition, delivery, clipboards, menu, floating panel, updater
app/extension/  The Chromium extension used to deliver into Chrome after clicking away
app/scripts/    Development, install, release and icon scripts
docs/icon/      The H mark's source artwork
artifacts/      Generated local builds and packages (ignored)
```

The one rule the code does not trade away is that dictated text is never lost:
`hvtt_core::pipeline::complete_transcription` saves a recovery draft, then copies to the clipboard,
then delivers, and its tests pin that order down.

## Build on macOS

Requirements:

- An Apple silicon Mac
- [Rust](https://rustup.rs) (stable) and CMake
- Xcode Command Line Tools (`xcode-select --install`)

From the repository root:

```bash
app/scripts/fetch-model.sh base.en   # the speech model, ~141 MB, into Application Support
app/scripts/dev.sh test
app/scripts/install.sh               # build, sign, install to ~/Applications, and open
```

`install.sh` signs with a local code-signing certificate named `Project Playground Local Signing`
when one exists (set `HVTT_SIGN_IDENTITY` to use another), so macOS keeps Microphone and
Accessibility permission across rebuilds. Without one it signs ad hoc, and permissions reset with
every rebuild.

Create and validate the unsigned beta DMG and its checksum:

```bash
app/scripts/release.sh --unsigned-beta
```

Generated files are written below `artifacts/macos/` and are not committed.

## Build on Windows

Requirements:

- Windows 10 or 11, 64-bit
- [Rust](https://rustup.rs) (stable, MSVC)
- Visual Studio 2022 Build Tools with **Desktop development with C++**, plus the **C++ CMake tools**
  and **C++ Clang tools** components
- [Inno Setup 6](https://jrsoftware.org/isinfo.php), for the installer only

From the repository root, in PowerShell:

```powershell
app\scripts\fetch-model.ps1          # the speech model, ~141 MB, into %LOCALAPPDATA%
app\scripts\dev.ps1 test
app\scripts\install.ps1              # build and install for this PC, and start it
app\scripts\release.ps1 -UnsignedBeta  # the public installer and its checksum
```

`install.ps1` builds for the processor it runs on. `release.ps1` builds for any AVX2 processor,
links the C runtime statically, and refuses to package an executable that carries a build path or
an email address. Generated files are written below `artifacts/windows/`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

The public/free version of Huck's Voice to Text is licensed under the
[GNU General Public License version 3](LICENSE).

Copyright (C) 2026 Quintin Huckaby

This license applies only to the files published in this public repository. It does not apply to
separate unpublished software.

Third-party components keep their own licenses: whisper.cpp and the Whisper model weights (MIT),
Tauri (MIT / Apache-2.0), and the Rust crates listed in [`app/README.md`](app/README.md).
