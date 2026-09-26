//! Huck's Voice to Text — desktop application.
//!
//! Hotkey -> record -> transcribe locally -> clipboard -> the text box that was focused at the
//! keypress. The clipboard copy always happens first; it is the safeguard when the box is gone.

pub mod bridge;
pub mod clip;
pub mod destination;
pub mod engine_whisper;
pub mod recorder;
pub mod update;
#[cfg(windows)]
mod win_shortcut;
#[cfg(windows)]
mod win_surface;

/// The gated paste rung, whichever platform this is.
#[cfg(target_os = "macos")]
use crate::destination::macos_paste as platform_paste;
#[cfg(windows)]
use crate::destination::windows_paste as platform_paste;

/// The normal clipboard's paste, as the keyboard labels it.
const NORMAL_PASTE: &str = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };
const OS_NAME: &str = if cfg!(target_os = "macos") { "macOS" } else { "Windows" };

/// Open a folder or file the way double-clicking it would.
fn open_path(path: std::path::PathBuf) {
    // Waited on, off the main thread: a spawned child that is never waited for lingers as a
    // finished-but-unreaped process until the app quits.
    std::thread::spawn(move || {
        let opener = if cfg!(windows) { "explorer.exe" } else { "/usr/bin/open" };
        let _ = std::process::Command::new(opener).arg(path).status();
    });
}

use crate::bridge::Bridge;
use crate::destination::PinError;
use hvtt_core::drafts::DraftStore;
use hvtt_core::pipeline::Destination;
use hvtt_core::engine::{Transcriber, TranscriptionRequest};
use hvtt_core::session::SessionState;
use hvtt_core::settings::{ClipboardChoice, Settings};
use hvtt_core::transcript::Transcript;
use parking_lot::Mutex;
use serde::Serialize;
use std::str::FromStr;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Shortcut, ShortcutState};

/// Everything the UI needs to draw itself, in one message.
#[derive(Debug, Clone, Serialize, Default)]
struct Snapshot {
    state: String,
    message: String,
    text: String,
    /// Recognition engine label, or why there isn't one.
    engine: String,
    engine_ready: bool,
    /// Peak microphone level, 0.0..=1.0.
    level: f32,
    shortcut: String,
    elapsed_ms: u128,
    /// The pinned destination's label, or None. Drives the small "sending to X" indicator.
    pinned: Option<String>,
    /// One short line when pinning is not possible. Never a dialog.
    pin_note: Option<String>,
    /// The words arrived in the text box, so the box is about to put itself away.
    delivered: bool,
    /// Show the one-time Accessibility ask. Once per launch, never twice.
    ask_permission: bool,
    /// The shortcut as the keyboard labels it, e.g. "Option + Space".
    shortcut_label: String,
    /// Set when the shortcut could not be registered, so it is never a silent failure.
    shortcut_error: Option<String>,
    /// "system" or "huck": which clipboard dictations are copied to.
    clipboard: ClipboardChoice,
    /// The paste-last-dictation shortcut as the keyboard labels it.
    paste_shortcut_label: String,
    paste_shortcut_error: Option<String>,
    /// Waiting in the box for new keys, chosen from the menu.
    rebinding: Option<Rebinding>,
    rebind_error: Option<String>,
    /// Check for Updates, while it is being shown.
    update: Option<UpdateView>,
    /// Perceived-latency budget, measured rather than assumed.
    timings: Timings,
}

/// The product's latency numbers. Measured every session so a regression shows up in normal use
/// rather than in a benchmark nobody runs.
#[derive(Debug, Clone, Default, Serialize)]
struct Timings {
    /// Shortcut pressed -> indicator on screen. Budget: under 150 ms.
    shortcut_to_visible_ms: u128,
    /// Shortcut pressed -> microphone actually capturing.
    shortcut_to_capture_ms: u128,
    /// Recognition wall time.
    transcribe_ms: u128,
    /// Clipboard + delivery.
    deliver_ms: u128,
}

struct App {
    session: Mutex<SessionState>,
    transcript: Mutex<Transcript>,
    recording: Mutex<Option<recorder::Recording>>,
    engine: Mutex<Option<Arc<dyn Transcriber>>>,
    engine_status: Mutex<String>,
    settings: Mutex<Settings>,
    drafts: Option<DraftStore>,
    message: Mutex<String>,
    level: Mutex<f32>,
    /// Set when the global shortcut could not be registered — almost always another app owns it.
    shortcut_error: Mutex<Option<String>>,
    paste_shortcut_error: Mutex<Option<String>>,
    rebinding: Mutex<Option<Rebinding>>,
    rebind_error: Mutex<Option<String>>,
    /// Windows: the keyboard hook that hears the new shortcut while the prompt is open.
    #[cfg(windows)]
    key_capture: Mutex<Option<win_shortcut::Capture>>,
    /// Windows: the window he was in when the pointer reached the H, given the foreground back
    /// when a menu item is chosen.
    #[cfg(windows)]
    before_menu: Mutex<Option<isize>>,
    update: Mutex<Option<UpdateView>>,
    /// A downloaded, verified update - the DMG on macOS, the installer on Windows - waiting to
    /// be opened.
    update_file: Mutex<Option<std::path::PathBuf>>,
    /// What the menu was last built from; it is rebuilt only when this changes.
    menu_key: Mutex<String>,
    /// Accessibility, as last checked. Asking macOS is a call to its permission service, so it
    /// is refreshed when the pointer reaches the H and per dictation - never per level update.
    ax_trusted: Mutex<bool>,
    elapsed_ms: Mutex<u128>,
    /// The pinned destination. `None` means clipboard-only, which is a normal outcome.
    destination: Mutex<Option<Box<dyn Destination>>>,
    pin_note: Mutex<Option<String>>,
    delivered: Mutex<bool>,
    ask_permission: Mutex<bool>,
    /// Set the first time the permission is found missing, so the ask happens once per launch.
    /// macOS only: Windows asks for no permission.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    permission_asked: Mutex<bool>,
    /// Bumped by every new dictation and every dismiss, so a delayed auto-hide never puts away a
    /// box that is showing something newer than what it was scheduled for.
    generation: Mutex<u64>,
    timings: Mutex<Timings>,
    bridge: Bridge,
}

impl App {
    fn snapshot(&self) -> Snapshot {
        let session = self.session.lock();
        // ONE settings lock. `parking_lot::Mutex` is not reentrant, so locking it twice while
        // building this struct deadlocks the thread - which silently stopped the engine from
        // loading and made the hotkey look like it was never registered.
        let (shortcut, clipboard, paste_shortcut) = {
            let s = self.settings.lock();
            (s.shortcut.clone(), s.clipboard, s.paste_shortcut.clone())
        };
        Snapshot {
            state: session.name().to_string(),
            message: self.message.lock().clone(),
            text: self.transcript.lock().text().to_string(),
            engine: self.engine_status.lock().clone(),
            engine_ready: self.engine.lock().is_some(),
            level: *self.level.lock(),
            shortcut: shortcut.clone(),
            elapsed_ms: *self.elapsed_ms.lock(),
            pinned: self.destination.lock().as_ref().map(|d| d.label()),
            pin_note: self.pin_note.lock().clone(),
            delivered: *self.delivered.lock(),
            ask_permission: *self.ask_permission.lock(),
            shortcut_label: hvtt_core::settings::describe_shortcut(&shortcut),
            shortcut_error: self.shortcut_error.lock().clone(),
            clipboard,
            paste_shortcut_label: hvtt_core::settings::describe_shortcut(&paste_shortcut),
            paste_shortcut_error: self.paste_shortcut_error.lock().clone(),
            rebinding: *self.rebinding.lock(),
            rebind_error: self.rebind_error.lock().clone(),
            update: self.update.lock().clone(),
            timings: self.timings.lock().clone(),
        }
    }

    fn set_state(&self, next: SessionState) {
        let mut cur = self.session.lock();
        match cur.transition_to(next.clone()) {
            Ok(s) => *cur = s,
            Err(e) => {
                // A rejected transition is a bug, not a user problem. Log it and hold.
                eprintln!("[hvtt] illegal transition ignored: {e}");
            }
        }
    }
}

/// One line per dictation, so the latency budgets are checked by using the product rather than by
/// running a benchmark nobody runs.
fn log_latency(state: &App) {
    let t = state.timings.lock();
    eprintln!(
        "[hvtt] latency: shortcut->visible {}ms | shortcut->capture {}ms | transcribe {}ms | deliver {}ms",
        t.shortcut_to_visible_ms, t.shortcut_to_capture_ms, t.transcribe_ms, t.deliver_ms
    );
}

fn push(app: &AppHandle) {
    let state: State<App> = app.state();
    let _ = app.emit("hvtt:update", state.snapshot());
    refresh_menu(app);
}

/// Bring the composer into view without taking the foreground.
///
/// `show()` alone is correct on macOS: the app runs as an accessory, so showing a window does
/// not activate it. Windows has no such policy, so the box is shown without activation by hand.
/// The never-take-the-foreground rule depends on nothing here calling `set_focus()`.
fn reveal_composer(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("composer") {
        #[cfg(windows)]
        win_surface::show(&w);
        #[cfg(not(windows))]
        let _ = w.show();
    }
}

fn hide_composer(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("composer") {
        #[cfg(windows)]
        win_surface::hide(&w);
        #[cfg(not(windows))]
        let _ = w.hide();
    }
}

/// Put the box away after `delay`, unless something newer has happened in it since.
///
/// Used only where the words are already safe elsewhere - typed into the field - or where there
/// were never any words to keep.
fn hide_after(app: &AppHandle, delay: std::time::Duration) {
    let state: State<App> = app.state();
    let scheduled_for = *state.generation.lock();
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let state: State<App> = app.state();
        if *state.generation.lock() == scheduled_for && !state.session.lock().is_capturing() {
            dismiss(app.clone());
        }
    });
}


// ---------------------------------------------------------------------------- pinning


// ---------------------------------------------------------------------------- the shortcut

/// What a global shortcut does.
#[derive(Clone, Copy)]
enum Action {
    /// Start or stop dictation.
    Dictate,
    /// Paste the last dictation from Huck's own clipboard.
    PasteLast,
}

/// Register `accelerator`, reporting a clash instead of failing quietly.
///
/// A shortcut that silently did not register is the worst outcome: the product's entire promise
/// is that one key press starts dictation, so the failure has to be visible and named.
fn register_shortcut(app: &AppHandle, accelerator: &str, action: Action) -> Result<(), String> {
    use hvtt_core::settings::{describe_shortcut, shortcut_looks_valid};
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    if !shortcut_looks_valid(accelerator) {
        return Err(format!(
            "{} needs a normal key as well as modifiers.",
            describe_shortcut(accelerator)
        ));
    }
    let parsed = Shortcut::from_str(accelerator)
        .map_err(|_| format!("{} isn't a shortcut {OS_NAME} understands.", describe_shortcut(accelerator)))?;

    // `on_shortcut` attaches the handler to this specific binding. The plugin's global
    // `with_handler` did not fire for shortcuts registered separately, which is the kind of
    // silent failure this product must never ship.
    app.global_shortcut()
        .on_shortcut(parsed, move |handle, _shortcut, event| {
            // Windows: a shortcut holding Alt (Alt+Space) would otherwise leave the app
            // underneath opening its menu bar when Alt comes up.
            #[cfg(windows)]
            if event.state() == ShortcutState::Pressed {
                platform_paste::mask_menu_key();
            }
            // Dictation fires on press - the start is the most felt moment in the product.
            // The paste fires as the letter key comes up: macOS drops a synthetic V while the
            // real V is still held, so a paste sent on press never arrived (measured
            // 2026-09-25). Control and Option may still be down; they do not leak.
            match (action, event.state()) {
                (Action::Dictate, ShortcutState::Pressed) => toggle(handle.clone()),
                (Action::PasteLast, ShortcutState::Released) => paste_last(),
                _ => {}
            }
        })
        .map_err(|_| {
        format!(
            "{} is already used by another app. Pick a different one in the H menu › Settings › Shortcuts.",
            describe_shortcut(accelerator)
        )
    })
}


/// Bind every shortcut the settings call for, starting from nothing, and record any clash.
///
/// One place decides what is bound, so switching the clipboard, rebinding and resetting all end
/// the same way.
fn bind_all(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    let _ = app.global_shortcut().unregister_all();
    let s = state.settings.lock().clone();
    let dictate = register_shortcut(app, &s.shortcut, Action::Dictate).err();
    // Only with Huck's clipboard: on the normal one, Cmd+V already does the job.
    let paste = match s.clipboard {
        ClipboardChoice::Huck => register_shortcut(app, &s.paste_shortcut, Action::PasteLast).err(),
        ClipboardChoice::System => None,
    };
    if let Some(why) = &dictate {
        eprintln!("[hvtt] {why}");
    }
    *state.shortcut_error.lock() = dictate;
    *state.paste_shortcut_error.lock() = paste;
}

/// Change a setting and save it. The menu rebuilds itself from the next update.
fn update_settings(app: &AppHandle, change: impl FnOnce(&mut Settings)) {
    let state: State<App> = app.state();
    let mut s = state.settings.lock().clone();
    change(&mut s);
    if let Err(e) = s.save() {
        eprintln!("[hvtt] settings not saved: {e}");
    }
    *state.settings.lock() = s;
}

/// Which shortcut is being rebound while the box waits for keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Rebinding {
    Dictation,
    Paste,
}

/// From the menu: open the box and wait for the new keys - Snip 'n' Clip's capture prompt.
///
/// Every global shortcut is released first, or pressing the current one would start a dictation
/// instead of reaching the box. This is the one time the box takes focus: he asked for it, and
/// it cannot hear keys otherwise.
fn begin_rebind(app: &AppHandle, which: Rebinding) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    if matches!(*state.session.lock(), SessionState::Recording | SessionState::Transcribing) {
        return;
    }
    let _ = app.global_shortcut().unregister_all();
    *state.rebinding.lock() = Some(which);
    *state.rebind_error.lock() = None;
    if let Some(w) = app.get_webview_window("composer") {
        // Windows: a keyboard hook hears the keys, so the box never needs the keyboard - and
        // Alt+Space, which Windows keeps from any page, can be chosen. If the hook cannot start,
        // the box takes the keyboard as on macOS.
        #[cfg(windows)]
        {
            let handle = app.clone();
            let capture = win_shortcut::start(move |heard| match heard {
                win_shortcut::Heard::Keys(accelerator) => finish_rebind(handle.clone(), accelerator),
                win_shortcut::Heard::Cancel => dismiss(handle.clone()),
            });
            if capture.is_some() {
                win_surface::show(&w);
            } else {
                win_surface::show_for_keys(&w);
            }
            *state.key_capture.lock() = capture;
        }
        #[cfg(not(windows))]
        {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
    push(app);
}

/// The keys he pressed. A clash keeps the prompt open with the reason, so he can try again.
#[tauri::command]
fn finish_rebind(app: AppHandle, accelerator: String) {
    use hvtt_core::settings::describe_shortcut;
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    let Some(which) = *state.rebinding.lock() else { return };
    let (other, action) = {
        let s = state.settings.lock();
        match which {
            Rebinding::Dictation => (s.paste_shortcut.clone(), Action::Dictate),
            Rebinding::Paste => (s.shortcut.clone(), Action::PasteLast),
        }
    };
    let tried = if accelerator.eq_ignore_ascii_case(&other) {
        Err(format!("{} is Huck's other shortcut. Try another.", describe_shortcut(&accelerator)))
    } else {
        // Registering is the only reliable test for "another app already owns this".
        register_shortcut(&app, &accelerator, action).map(|()| {
            let _ = app.global_shortcut().unregister_all();
        })
    };
    match tried {
        Ok(()) => {
            update_settings(&app, |s| match which {
                Rebinding::Dictation => s.shortcut = accelerator.clone(),
                Rebinding::Paste => s.paste_shortcut = accelerator.clone(),
            });
            // Rebinds everything and puts the box away.
            dismiss(app.clone());
        }
        Err(why) => {
            *state.rebind_error.lock() = Some(why.replace(" Pick a different one in Settings.", ""));
            // The prompt stays open for another try, with its full time again.
            #[cfg(windows)]
            win_shortcut::extend();
            push(&app);
        }
    }
}

/// Paste the last dictation from Huck's clipboard into whatever he is typing in now.
///
/// This one is aimed by him, deliberately, at the field in front of him - so unlike delivery it
/// needs no pin. His normal clipboard is borrowed for the paste and put back afterwards.
fn paste_last() {
    #[cfg(any(target_os = "macos", windows))]
    std::thread::spawn(move || {
        let Some(text) = crate::clip::huck::read().filter(|t| !t.trim().is_empty()) else {
            return;
        };
        // Immediately, with his fingers still on the keys, on both platforms: see each
        // platform's `press_paste`.
        let _ = platform_paste::paste_borrowing_clipboard(&text);
    });
}

// ---------------------------------------------------------------------------- updates

/// What the floating box says during Check for Updates.
#[derive(Debug, Clone, Serialize)]
struct UpdateView {
    /// checking | current | downloading | ready | failed
    stage: &'static str,
    title: String,
    detail: String,
}

fn show_update(app: &AppHandle, stage: &'static str, title: String, detail: String) {
    let state: State<App> = app.state();
    *state.update.lock() = Some(UpdateView { stage, title, detail });
    // Dictation outranks this: it waits in the state and shows when the box is next free.
    if !matches!(*state.session.lock(), SessionState::Recording | SessionState::Transcribing) {
        reveal_composer(app);
    }
    push(app);
}

/// Settings › Check for Updates…. The only network connection the program makes, and only now.
fn check_for_updates(app: &AppHandle) {
    let state: State<App> = app.state();
    if matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading")) {
        reveal_composer(app);
        return;
    }
    *state.update_file.lock() = None;
    show_update(app, "checking", "Checking for updates…".into(), "Asking GitHub.".into());
    let app = app.clone();
    std::thread::spawn(move || {
        let current = app.package_info().version.to_string();
        let fail = |why: String| show_update(&app, "failed", "Couldn't update".into(), why);
        match update::fetch_latest(&current) {
            Err(why) => fail(why),
            Ok(update::Check::UpToDate { .. }) => {
                show_update(
                    &app,
                    "current",
                    "You're up to date".into(),
                    format!("Version {current} is the latest."),
                );
                hide_after(&app, std::time::Duration::from_millis(2600));
            }
            Ok(update::Check::Available(offer)) => {
                show_update(
                    &app,
                    "downloading",
                    format!("Downloading version {}…", offer.version),
                    "It is checked against its published SHA-256 before it is offered.".into(),
                );
                match update::download(&offer) {
                    Err(why) => fail(why),
                    Ok(file) => {
                        *app.state::<App>().update_file.lock() = Some(file);
                        let how = if cfg!(windows) {
                            "Open it and the installer takes over; Huck's Voice to Text closes \
                             for it and your settings stay."
                        } else {
                            "Open it, quit Huck's Voice to Text, and drag the new copy onto \
                             Applications."
                        };
                        show_update(
                            &app,
                            "ready",
                            format!("Version {} is ready", offer.version),
                            how.into(),
                        );
                    }
                }
            }
        }
    });
}

/// macOS: open the verified DMG in Finder - it never installs over itself; he drags the new copy
/// across. Windows: start the verified installer and step aside, as Snip 'n' Clip does, because a
/// running program's files cannot be replaced.
#[tauri::command]
fn open_update(app: AppHandle) {
    let file = app.state::<App>().update_file.lock().clone();
    // Snip 'n' Clip's way: a silent install that closes this copy and starts the new one.
    #[cfg(windows)]
    if let Some(installer) = file {
        // The installer must not find this copy still "running" while it quits.
        win_surface::release_single_instance();
        if std::process::Command::new(installer).args(["/SILENT", "/CLOSEAPPLICATIONS"]).spawn().is_ok() {
            app.exit(0);
            return;
        }
    }
    #[cfg(not(windows))]
    if let Some(dmg) = file {
        open_path(dmg);
    }
    dismiss(app);
}

// ---------------------------------------------------------------------------- the menu

/// The menu bar H is where every setting lives, the way it does in Huck's Snip 'n' Clip. There is
/// no settings window: a program that is a layer over other work should not open one.
const TRAY: &str = "hvtt";

/// Everything the menu shows. It is rebuilt only when this changes - not on every level update.
fn menu_key(state: &App) -> String {
    let s = state.settings.lock().clone();
    format!(
        "{}|{}|{}|{:?}|{}|{}|{}|{:?}|{:?}|{:?}",
        state.session.lock().is_capturing(),
        s.shortcut,
        s.paste_shortcut,
        s.clipboard,
        s.keep_drafts,
        *state.ax_trusted.lock(),
        state.engine_status.lock(),
        state.shortcut_error.lock(),
        state.paste_shortcut_error.lock(),
        state.rebinding.lock(),
    )
}

fn refresh_menu(app: &AppHandle) {
    let state: State<App> = app.state();
    let key = menu_key(&state);
    {
        let mut last = state.menu_key.lock();
        if *last == key {
            return;
        }
        *last = key;
    }
    let app2 = app.clone();
    // Menus belong to the main thread on macOS.
    let _ = app.run_on_main_thread(move || match build_menu(&app2) {
        Ok(menu) => {
            if let Some(tray) = app2.tray_by_id(TRAY) {
                let _ = tray.set_menu(Some(menu));
            }
        }
        Err(e) => eprintln!("[hvtt] menu not built: {e}"),
    });
}

/// Snip 'n' Clip's layout: name, actions with their keys, then Settings, then Quit.
fn build_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use hvtt_core::settings::describe_shortcut as keys;
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

    let state: State<App> = app.state();
    let s = state.settings.lock().clone();
    let recording = state.session.lock().is_capturing();
    let huck = s.clipboard == ClipboardChoice::Huck;
    let taken = |error: &Mutex<Option<String>>| if error.lock().is_some() { " (in use by another app)" } else { "" };
    let item = |id: &str, text: String, enabled: bool| {
        MenuItem::with_id(app, id, text, enabled, None::<&str>)
    };
    let check = |id: &str, text: &str, on: bool| {
        CheckMenuItem::with_id(app, id, text, true, on, None::<&str>)
    };

    let menu = Menu::new(app)?;
    menu.append(&item("heading", "Huck's Voice to Text".into(), false)?)?;
    menu.append(&item("brand", "Powered by Project Playground".into(), false)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    if !*state.ax_trusted.lock() {
        menu.append(&item("allow-ax", "Allow Accessibility…".into(), true)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    let verb = if recording { "Stop Dictation" } else { "Start Dictation" };
    menu.append(&item("dictate", format!("{verb} — {}", keys(&s.shortcut)), true)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let shortcuts = Submenu::with_id(app, "shortcuts", "Shortcuts", !recording)?;
    shortcuts.append(&item(
        "rebind-dictation",
        format!("Dictation — {}{}", keys(&s.shortcut), taken(&state.shortcut_error)),
        true,
    )?)?;
    // Always listed, so the key is there to see and change; it works while Huck's clipboard is
    // the choice.
    shortcuts.append(&item(
        "rebind-paste",
        format!("Huck's Clipboard — {}{}", keys(&s.paste_shortcut), taken(&state.paste_shortcut_error)),
        true,
    )?)?;
    shortcuts.append(&PredefinedMenuItem::separator(app)?)?;
    shortcuts.append(&item("reset-shortcuts", "Reset Shortcuts to Defaults".into(), true)?)?;

    let clipboard = Submenu::with_id(app, "clipboard", "Clipboard", true)?;
    clipboard.append(&check("clip-system", &format!("Normal Clipboard — {NORMAL_PASTE}"), !huck)?)?;
    clipboard.append(&check(
        "clip-huck",
        &format!("Huck's Clipboard — {}", keys(&s.paste_shortcut)),
        huck,
    )?)?;

    let settings = Submenu::with_id(app, "settings", "Settings", true)?;
    settings.append(&shortcuts)?;
    settings.append(&clipboard)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    settings.append(&check("keep-drafts", "Keep Recovery Drafts", s.keep_drafts)?)?;
    settings.append(&item("open-drafts", "Open Drafts Folder".into(), true)?)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    // "ggml-base.en.bin" is a file name; "base.en" is the model.
    let model = state.engine_status.lock().trim_start_matches("ggml-").trim_end_matches(".bin").to_string();
    settings.append(&item("model", format!("Speech Model — {model}"), false)?)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    settings.append(&item("check-updates", "Check for Updates…".into(), !recording)?)?;
    settings.append(&item("version", format!("Version {}", app.package_info().version), false)?)?;
    menu.append(&settings)?;

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, Some("CmdOrCtrl+Q"))?)?;
    Ok(menu)
}

fn on_menu(app: &AppHandle, id: &str) {
    let state: State<App> = app.state();
    // Windows leaves the menu's hidden window in front; give it back to where he was first.
    #[cfg(windows)]
    if let Some(h) = *state.before_menu.lock() {
        win_surface::give_back_foreground(h);
    }
    match id {
        // Windows: a moment for that window to take its focus back, so the dictation is aimed at
        // the text box he was in - as it is when the menu-bar H starts one on macOS.
        #[cfg(windows)]
        "dictate" => {
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                let again = app.clone();
                let _ = app.run_on_main_thread(move || toggle(again));
            });
        }
        #[cfg(not(windows))]
        "dictate" => toggle(app.clone()),
        "allow-ax" => open_accessibility_settings(),
        "check-updates" => check_for_updates(app),
        "rebind-dictation" => begin_rebind(app, Rebinding::Dictation),
        "rebind-paste" => begin_rebind(app, Rebinding::Paste),
        "reset-shortcuts" => {
            update_settings(app, |s| {
                s.shortcut = hvtt_core::settings::DEFAULT_SHORTCUT.to_string();
                s.paste_shortcut = hvtt_core::settings::DEFAULT_PASTE_SHORTCUT.to_string();
            });
            bind_all(app);
        }
        "clip-system" | "clip-huck" => {
            let choice = if id == "clip-huck" { ClipboardChoice::Huck } else { ClipboardChoice::System };
            update_settings(app, |s| s.clipboard = choice);
            bind_all(app);
        }
        "keep-drafts" => update_settings(app, |s| s.keep_drafts = !s.keep_drafts),
        "open-drafts" => {
            if let Some(dir) = hvtt_core::paths::drafts_dir() {
                let _ = std::fs::create_dir_all(&dir);
                open_path(dir);
            }
        }
        "quit" => app.exit(0),
        _ => return,
    }
    // A check item flips its own tick when clicked; rebuild so it always shows the setting.
    state.menu_key.lock().clear();
    push(app);
}

/// True when Accessibility has been granted. Used to decide whether to show the one-time ask.
#[tauri::command]
fn accessibility_ready() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::destination::macos_ax::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Open the exact System Settings pane, so the permission ask is one click, not a treasure hunt.
#[tauri::command]
fn open_accessibility_settings() {
    // Without this the app is not in the list at all, and the pane opens with nothing to tick.
    // Windows needs no grant for UI Automation, so there is nothing to open there.
    #[cfg(target_os = "macos")]
    {
        crate::destination::macos_ax::request_accessibility();
        open_path("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility".into());
    }
}

// ---------------------------------------------------------------------------- commands

#[tauri::command]
fn get_snapshot(state: State<App>) -> Snapshot {
    state.snapshot()
}

/// The hotkey and the on-screen button both land here.
#[tauri::command]
fn toggle(app: AppHandle) {
    let state: State<App> = app.state();
    if state.rebinding.lock().is_some() {
        return;
    }
    let is_recording = state.session.lock().is_capturing();
    if is_recording {
        stop_and_transcribe(app.clone());
    } else {
        start_recording(app.clone());
    }
}

/// Put the box away. Always available, in every state, so the box can never get stuck on screen.
///
/// Mid-recording this abandons the recording; there are no words yet to lose.
#[tauri::command]
fn dismiss(app: AppHandle) {
    let state: State<App> = app.state();
    if matches!(*state.session.lock(), SessionState::Transcribing) {
        // ~100 ms of work whose result is about to be copied and delivered.
        return;
    }
    *state.generation.lock() += 1;
    // Leaving the key prompt, by any route, puts every shortcut back.
    if state.rebinding.lock().take().is_some() {
        *state.rebind_error.lock() = None;
        // Let go of the keyboard before the shortcuts go back on.
        #[cfg(windows)]
        {
            *state.key_capture.lock() = None;
        }
        bind_all(&app);
    }
    if let Some(rec) = state.recording.lock().take() {
        drop(rec.finish());
    }
    *state.level.lock() = 0.0;
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading")) {
        *state.update.lock() = None;
    }
    *state.transcript.lock() = Transcript::empty();
    *state.message.lock() = String::new();
    *state.elapsed_ms.lock() = 0;
    state.set_state(SessionState::Idle);
    // The dictation is over, so let go of where it went. On Windows this is also what ends the
    // click and key count behind the paste gate, which runs only while a dictation needs it.
    #[cfg(windows)]
    {
        *state.destination.lock() = None;
    }
    hide_composer(&app);
    push(&app);
}


#[tauri::command]
fn get_settings(state: State<App>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let state: State<App> = app.state();
    settings.save().map_err(|e| e.to_string())?;
    *state.settings.lock() = settings;
    push(&app);
    Ok(())
}

/// Where the recovery drafts are, so the user can always find their words.
#[tauri::command]
fn drafts_dir() -> Option<String> {
    hvtt_core::paths::drafts_dir().map(|p| p.display().to_string())
}

// ---------------------------------------------------------------------------- pipeline

fn start_recording(app: AppHandle) {
    let pressed = std::time::Instant::now();
    let state: State<App> = app.state();

    if state.engine.lock().is_none() {
        let why = state.engine_status.lock().clone();
        *state.message.lock() = why.clone();
        state.set_state(SessionState::Error { message: why });
        reveal_composer(&app);
        push(&app);
        return;
    }

    // 0. CAPTURE THE DESTINATION AT THE KEYPRESS — before anything else.
    //
    // The pin must mean "the field that was focused when I pressed the shortcut". Reading focus
    // later reads a different field, because by then the user has clicked away — which is the
    // whole workflow this product is for. The Accessibility capture is a handful of C calls and
    // costs microseconds, so it fits ahead of the indicator; validating it does not, and
    // happens later from the snapshot alone.
    #[cfg(target_os = "macos")]
    let pending = hvtt_core::pinning::PendingPin::capture(
        &crate::destination::macos_ax::AxFocusSource,
    );
    // What was in front and how much he had touched, for the paste rung's "nothing moved" gate.
    // On Windows this is the whole keypress capture: the focused element is read from UI
    // Automation on a worker a moment later, and refused if anything moved in between.
    #[cfg(any(target_os = "macos", windows))]
    let stamp = platform_paste::FocusStamp::capture();

    // The browser's own pin has to be taken at the same instant, so the request is sent now and
    // waited for later. The extension pins its focused element the moment this arrives.
    let browser_pin = {
        let bridge = state.bridge.clone();
        std::thread::spawn(move || crate::destination::chromium::ChromiumDestination::pin(bridge))
    };

    // 1. THE INDICATOR, before anything that can block. The budget is 150 ms from keypress to
    //    visible; it is the most felt number in the product.
    // A new dictation starts empty. The previous words are already in their text box, or on
    // the clipboard and in the recovery draft; appending them would send old words somewhere new.
    *state.generation.lock() += 1;
    *state.transcript.lock() = Transcript::empty();
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    // A finished update message gives way; a check still running re-shows itself when done.
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading")) {
        *state.update.lock() = None;
    }
    state.set_state(SessionState::Recording);
    *state.message.lock() = "Listening…".into();
    *state.elapsed_ms.lock() = 0;
    *state.pin_note.lock() = None;
    *state.destination.lock() = None;
    reveal_composer(&app);
    state.timings.lock().shortcut_to_visible_ms = pressed.elapsed().as_millis();
    push(&app);

    // 2. Microphone, so the first words are not lost while the destination is worked out.
    let device = state.settings.lock().input_device.clone();
    match recorder::Recording::start(device.as_deref()) {
        Ok(rec) => {
            *state.recording.lock() = Some(rec);
            state.timings.lock().shortcut_to_capture_ms = pressed.elapsed().as_millis();
            spawn_level_pump(app.clone());
        }
        Err(e) => {
            let settings = if cfg!(windows) {
                "Settings › Privacy › Microphone"
            } else {
                "System Settings › Privacy"
            };
            let msg = format!("{e} Check microphone access in {settings}.");
            *state.message.lock() = msg.clone();
            state.set_state(SessionState::Error { message: msg });
            push(&app);
            return;
        }
    }

    // 3. Validation, off the critical path, using ONLY what was captured in step 0.
    let app2 = app.clone();
    std::thread::spawn(move || {
        let browser = browser_pin.join().ok();
        #[cfg(target_os = "macos")]
        resolve_pin(&app2, pending, stamp, browser);
        #[cfg(windows)]
        resolve_pin_windows(&app2, stamp, browser);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = browser;
        push(&app2);
    });
}

/// Windows' half of `resolve_pin`: the same three ways in, best first - UI Automation (native
/// apps, which works even after he clicks away), the browser extension (Chrome, Edge), and a
/// paste (Electron apps, Chrome without the extension, anything UI Automation cannot write),
/// made only if nothing has moved since the keypress. No permission step: UI Automation needs
/// no grant on Windows.
#[cfg(windows)]
fn resolve_pin_windows(
    app: &AppHandle,
    stamp: Option<crate::destination::windows_paste::FocusStamp>,
    browser: Option<Result<crate::destination::chromium::ChromiumDestination, String>>,
) {
    use crate::destination::windows_paste::PasteDestination;
    use crate::destination::{is_chromium_executable, is_unsupported_executable, windows_uia};

    let state: State<App> = app.state();
    let set = |d: Box<dyn Destination>| {
        *state.message.lock() = format!("Listening — will send to {}", d.label());
        *state.destination.lock() = Some(d);
    };
    let note = |e: PinError| {
        *state.pin_note.lock() = Some(e.message());
        if !e.is_expected() {
            *state.message.lock() = e.message();
        }
    };
    let release_browser = || {
        let _ = state.bridge.request("unpin", None, std::time::Duration::from_millis(300));
    };

    // Nothing to type into was in front: the desktop, the taskbar, or the H's own menu.
    let Some(stamp) = stamp else {
        if let Some(Ok(_)) = browser {
            release_browser();
        }
        note(PinError::NotATextField);
        return;
    };

    let exe = windows_uia::executable_of(stamp.pid());
    let app_label = is_unsupported_executable(&exe).unwrap_or_else(|| windows_uia::app_name(&exe));
    let borrow = state.settings.lock().clipboard == ClipboardChoice::Huck;
    let paste = || PasteDestination::new(stamp.clone(), app_label.clone(), borrow);

    // The extension is the only silent way into Chrome.
    if is_chromium_executable(&exe) {
        if let Some(Ok(d)) = browser {
            set(Box::new(d));
            return;
        }
    } else if let Some(Ok(_)) = browser {
        // Not our destination; release it so the extension is not left holding a field.
        release_browser();
    }

    // Chromium and Electron windows are never asked for their accessibility tree: they have no
    // native fields to write, and asking flips some (VS Code) into screen-reader mode. Paste.
    if stamp.is_chromium_window() || is_chromium_executable(&exe) || is_unsupported_executable(&exe).is_some() {
        set(Box::new(paste()));
        return;
    }

    match windows_uia::capture(&stamp).and_then(|el| windows_uia::validate_captured(el, app_label.clone())) {
        Ok(d) => set(Box::new(d.with_paste_fallback(Some(paste())))),
        Err(reason) if reason == "secure-field" => note(PinError::SecureField),
        // Nothing UI Automation could write: paste, gated. That includes focus having moved
        // before the capture landed - the gate then refuses at delivery, just as it would had he
        // moved later, and the words wait on the clipboard.
        Err(_) => set(Box::new(paste())),
    }
}

/// Decide what the captured candidate actually is, and refuse rather than substitute.
///
/// Three ways in, best first: a silent Accessibility write (native apps and Safari, which works
/// even after he clicks away), the browser extension (Chrome), and a paste (everything else),
/// which is only made if nothing has moved since the keypress. The clipboard already has the
/// words whichever way this goes.
#[cfg(target_os = "macos")]
fn resolve_pin(
    app: &AppHandle,
    pending: hvtt_core::pinning::PendingPin<crate::destination::macos_ax::AxElement>,
    stamp: Option<crate::destination::macos_paste::FocusStamp>,
    browser: Option<Result<crate::destination::chromium::ChromiumDestination, String>>,
) {
    use crate::destination::macos_paste::PasteDestination;
    use crate::destination::{is_chromium_executable, is_unsupported_executable, macos_ax};
    use hvtt_core::pinning::PinResolution;

    let state: State<App> = app.state();

    // The front window's owner stands in when Accessibility shows no focused element, which is
    // how Electron apps look from outside.
    let exe = pending
        .candidate()
        .and_then(macos_ax::pid_of)
        .or(stamp.map(|s| s.pid()))
        .map(macos_ax::executable_of)
        .unwrap_or_default();
    let app_label = is_unsupported_executable(&exe).unwrap_or_else(|| {
        exe.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("that app").to_string()
    });
    let borrow = state.settings.lock().clipboard == ClipboardChoice::Huck;
    let paste = || stamp.map(|s| PasteDestination::new(s, app_label.clone(), borrow));

    let set = |d: Box<dyn Destination>| {
        *state.message.lock() = format!("Listening — will send to {}", d.label());
        *state.destination.lock() = Some(d);
    };
    let note = |e: PinError| {
        *state.pin_note.lock() = Some(e.message());
        if !e.is_expected() {
            *state.message.lock() = e.message();
        }
    };

    // The extension is the only silent way into Chrome, and it needs no macOS permission.
    if is_chromium_executable(&exe) {
        if let Some(Ok(d)) = browser {
            set(Box::new(d));
            return;
        }
    } else if let Some(Ok(_)) = browser {
        // Not our destination; release it so the extension is not left holding a field.
        let _ = state
            .bridge
            .request("unpin", None, std::time::Duration::from_millis(300));
    }

    // Everything below writes or pastes into another app, and macOS allows neither without
    // Accessibility. Its own prompt adds the app to the list - the plain check never did, which
    // left nothing to switch on. Once per launch.
    let trusted = macos_ax::accessibility_trusted();
    *state.ax_trusted.lock() = trusted;
    if !trusted {
        if !std::mem::replace(&mut *state.permission_asked.lock(), true) {
            *state.ask_permission.lock() = true;
            macos_ax::request_accessibility();
        }
        note(PinError::AccessibilityPermissionMissing);
        return;
    }

    // Chrome without the extension and Electron apps have no silent write at all: paste.
    if is_chromium_executable(&exe) || is_unsupported_executable(&exe).is_some() {
        match paste() {
            Some(p) => set(Box::new(p)),
            None => note(PinError::Unsupported { app: app_label.clone() }),
        }
        return;
    }

    match pending.resolve(macos_ax::validate_captured) {
        Ok(d) => set(Box::new(d.with_paste_fallback(paste()))),
        Err(PinResolution::Rejected(reason)) if reason == "secure-field" => {
            note(PinError::SecureField)
        }
        // Nothing Accessibility could read, or not a text field it knows: paste, gated.
        Err(_) => match paste() {
            Some(p) => set(Box::new(p)),
            None => note(PinError::NotATextField),
        },
    }
}

/// Feed the listening animation while the microphone is open.
fn spawn_level_pump(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(60));
        let state: State<App> = app.state();
        let level = {
            let rec = state.recording.lock();
            match rec.as_ref() {
                Some(r) => r.take_level(),
                None => break,
            }
        };
        if !state.session.lock().is_capturing() {
            break;
        }
        *state.level.lock() = level;
        push(&app);
    });
}

fn stop_and_transcribe(app: AppHandle) {
    let state: State<App> = app.state();

    let samples = match state.recording.lock().take() {
        Some(rec) => rec.finish(),
        None => return,
    };
    *state.level.lock() = 0.0;

    if !hvtt_core::audio::is_long_enough(samples.len(), hvtt_core::audio::WHISPER_SAMPLE_RATE) {
        // A mis-press. Say so plainly and go back to resting without an error state.
        *state.message.lock() = "That was too short to transcribe.".into();
        state.set_state(SessionState::Transcribing);
        state.set_state(SessionState::Ready);
        push(&app);
        hide_after(&app, std::time::Duration::from_millis(1800));
        return;
    }

    state.set_state(SessionState::Transcribing);
    *state.message.lock() = "Transcribing…".into();
    push(&app);

    let engine = state.engine.lock().clone();
    let prompt = state.settings.lock().vocabulary_prompt();

    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let Some(engine) = engine else { return };

        let req = TranscriptionRequest { samples, vocabulary_prompt: prompt };

        match engine.transcribe(&req) {
            Ok(result) => {
                *state.elapsed_ms.lock() = result.elapsed_ms;
                state.timings.lock().transcribe_ms = result.elapsed_ms;

                if result.text.trim().is_empty() {
                    *state.message.lock() =
                        "No speech was recognised in that recording.".into();
                    state.set_state(SessionState::Ready);
                    log_latency(&state);
                    push(&app);
                    hide_after(&app, std::time::Duration::from_millis(1800));
                    return;
                }

                *state.transcript.lock() = Transcript::settled(result.text.trim().to_string());

                // The product rule, in one call: draft to disk, then clipboard, then delivery.
                // Nothing here loses the text, and the paste rung relies on the clipboard copy.
                let (choice, paste_key) = {
                    let s = state.settings.lock();
                    (s.clipboard, hvtt_core::settings::describe_shortcut(&s.paste_shortcut))
                };
                // One clipboard or the other, never both.
                let system = clip::SystemClipboard::new(app.clone());
                #[cfg(any(target_os = "macos", windows))]
                let huck = clip::huck::HuckClipboard;
                let clipboard: &dyn hvtt_core::pipeline::Clipboard = match choice {
                    #[cfg(any(target_os = "macos", windows))]
                    ClipboardChoice::Huck => &huck,
                    _ => &system,
                };
                let transcript = state.transcript.lock().clone();
                let deliver_started = std::time::Instant::now();
                let report = {
                    let dest = state.destination.lock();
                    hvtt_core::complete_transcription(
                        &transcript,
                        clipboard,
                        dest.as_deref(),
                        state.drafts.as_ref().filter(|_| state.settings.lock().keep_drafts),
                    )
                };
                state.timings.lock().deliver_ms = deliver_started.elapsed().as_millis();

                use hvtt_core::pipeline::{DeliveryError, DeliveryOutcome};
                let delivered = matches!(report.delivery, DeliveryOutcome::Delivered { .. });
                // Where the words wait, and the key that gets them back.
                let (kept, key) = match choice {
                    ClipboardChoice::Huck => ("on Huck's clipboard", paste_key),
                    ClipboardChoice::System => ("copied", NORMAL_PASTE.to_string()),
                };
                *state.message.lock() = match &report.delivery {
                    _ if !report.clipboard_ok => report.message.clone(),
                    DeliveryOutcome::NotAttempted => {
                        let mut kept = kept.to_string();
                        kept[..1].make_ascii_uppercase();
                        format!("{kept} — press {key} to paste.")
                    }
                    DeliveryOutcome::Failed { label, error: DeliveryError::DestinationLost, .. } => {
                        format!("Couldn't reach {label} — {kept}. Press {key} to paste.")
                    }
                    DeliveryOutcome::Failed { error: DeliveryError::RefusedSecureField, .. } => {
                        format!("That's a password box, so Huck left it alone — {kept}.")
                    }
                    DeliveryOutcome::Failed { label, .. } => {
                        format!("Couldn't type into {label} — {kept}. Press {key} to paste.")
                    }
                    _ => report.message.clone(),
                };
                *state.delivered.lock() = delivered;
                state.set_state(SessionState::Ready);
                log_latency(&state);
                push(&app);
                // No text box afterwards: the words are in the field, or on the clipboard. Say
                // which, briefly, and get out of the way. Two things hold the box up: a failed
                // clipboard (the words are only here and in the draft) and the one-time
                // permission ask, which needs a click.
                let hold = !report.clipboard_ok || *state.ask_permission.lock();
                if !hold {
                    let ms = if delivered { 1200 } else { 2600 };
                    hide_after(&app, std::time::Duration::from_millis(ms));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                *state.message.lock() = msg.clone();
                state.set_state(SessionState::Error { message: msg });
                push(&app);
            }
        }
    });
}

// ---------------------------------------------------------------------------- startup

/// Load the recognition model in the background so the window appears immediately.
fn spawn_engine_load(app: AppHandle) {
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let model_file = state.settings.lock().model.clone();

        let Some(path) = engine_whisper::WhisperEngine::expected_path(&model_file) else {
            *state.engine_status.lock() = "No application data directory available.".into();
            push(&app);
            return;
        };

        *state.engine_status.lock() = format!("Loading {model_file}…");
        push(&app);

        match engine_whisper::WhisperEngine::load(&path) {
            Ok(engine) => {
                *state.engine_status.lock() = engine.name().to_string();
                *state.engine.lock() = Some(Arc::new(engine));
            }
            Err(e) => {
                *state.engine_status.lock() = e.to_string();
            }
        }
        push(&app);
    });
}

pub fn run() {
    #[cfg(windows)]
    if !win_surface::claim_single_instance() {
        return;
    }
    let settings = Settings::load();

    let app_state = App {
        session: Mutex::new(SessionState::Idle),
        transcript: Mutex::new(Transcript::empty()),
        recording: Mutex::new(None),
        engine: Mutex::new(None),
        engine_status: Mutex::new("Starting…".into()),
        settings: Mutex::new(settings),
        drafts: DraftStore::at_default_location(),
        message: Mutex::new(String::new()),
        level: Mutex::new(0.0),
        shortcut_error: Mutex::new(None),
        paste_shortcut_error: Mutex::new(None),
        rebinding: Mutex::new(None),
        rebind_error: Mutex::new(None),
        #[cfg(windows)]
        key_capture: Mutex::new(None),
        #[cfg(windows)]
        before_menu: Mutex::new(None),
        update: Mutex::new(None),
        update_file: Mutex::new(None),
        menu_key: Mutex::new(String::new()),
        ax_trusted: Mutex::new(accessibility_ready()),
        elapsed_ms: Mutex::new(0),
        destination: Mutex::new(None),
        pin_note: Mutex::new(None),
        delivered: Mutex::new(false),
        ask_permission: Mutex::new(false),
        permission_asked: Mutex::new(false),
        generation: Mutex::new(0),
        timings: Mutex::new(Timings::default()),
        bridge: Bridge::new(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .build(),
        )
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            toggle,
            dismiss,
            get_settings,
            save_settings,
            drafts_dir,
            accessibility_ready,
            open_accessibility_settings,
            finish_rebind,
            open_update,
        ])
        .setup(move |app| {
            // An accessory app has no Dock icon and never steals the foreground when a window
            // is shown. This single line is what makes rule 1 achievable on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            // Windows has no such policy; the box carries never-activate styles instead.
            #[cfg(windows)]
            if let Some(w) = app.get_webview_window("composer") {
                win_surface::prepare(&w);
            }

            {
                let handle = app.handle().clone();
                bind_all(&handle);
                // The box starts hidden, so an error only it can show was a silent failure: the
                // app looked like it had not started at all.
                if app.state::<App>().shortcut_error.lock().is_some() {
                    reveal_composer(&handle);
                }
            }

            // The menu-bar H holds every setting, as in Snip 'n' Clip. A template image, so
            // macOS tints it to match the menu bar like every other item there. Windows does not
            // tint tray icons, so there it is Snip 'n' Clip's tray gray, which reads on a light
            // or a dark taskbar.
            {
                use tauri::tray::{TrayIconBuilder, TrayIconEvent};
                let menu = build_menu(app.handle())?;
                *app.state::<App>().menu_key.lock() = menu_key(&app.state::<App>());
                #[cfg(windows)]
                let icon = tauri::include_image!("icons/tray-win.png");
                #[cfg(not(windows))]
                let icon = tauri::include_image!("icons/tray@2x.png");
                TrayIconBuilder::with_id(TRAY)
                    .menu(&menu)
                    .show_menu_on_left_click(true)
                    .icon(icon)
                    .icon_as_template(true)
                    .tooltip("Huck's Voice to Text")
                    .on_tray_icon_event(|tray, event| {
                        // The pointer reaching the H is the moment to re-ask about Accessibility,
                        // so a grant made in System Settings shows before the menu opens.
                        if let TrayIconEvent::Enter { .. } = event {
                            let app = tray.app_handle();
                            let state: State<App> = app.state();
                            *state.ax_trusted.lock() = accessibility_ready();
                            refresh_menu(app);
                        }
                        // Windows: note where he was working before the click takes it away.
                        #[cfg(windows)]
                        if matches!(event, TrayIconEvent::Enter { .. } | TrayIconEvent::Move { .. }) {
                            if let Some(h) = win_surface::outside_foreground() {
                                *tray.app_handle().state::<App>().before_menu.lock() = Some(h);
                            }
                        }
                    })
                    .build(app)?;
                app.handle().on_menu_event(|app, event| on_menu(app, event.id.as_ref()));
            }

            // Listen for the browser extension's native-messaging host. No port, no polling:
            // it is a Unix socket in the app-data directory that only this user can open - on
            // Windows too, which has had them since Windows 10 1803.
            {
                let state: State<App> = app.state();
                if let Err(e) = state.bridge.serve() {
                    eprintln!("[hvtt] browser bridge unavailable: {e}");
                }
            }

            // The window server's first answer costs ~45 ms; pay it now, not on the first keypress.
            #[cfg(target_os = "macos")]
            std::thread::spawn(crate::destination::macos_paste::warm_up);

            spawn_engine_load(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Huck's Voice to Text");
}
