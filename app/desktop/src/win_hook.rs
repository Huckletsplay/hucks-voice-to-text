//! Low-level input hooks on a thread of their own, with an owner that can always stop them.
//!
//! Two parts of the program watch the keyboard for a short while: the paste gate (counting, while
//! a dictation is in flight) and the shortcut prompt (swallowing keys, while it is open). A hook
//! that outlived its owner would be dangerous - the prompt's would swallow every key until the
//! program quit - so the thread and its owner agree on ownership under one lock: if the owner
//! stops waiting before the hooks are in, the thread takes them straight out again and exits
//! (found in Codex's review, 2026-09-26: the first version simply lost track of a late thread).

use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HOOKPROC,
    MSG, PM_NOREMOVE, WINDOWS_HOOK_ID, WM_QUIT, WM_USER,
};

enum Handshake {
    Waiting,
    Running(u32),
    /// The owner stopped waiting; a thread that gets here late must not keep its hooks.
    Abandoned,
    Failed,
}

/// Hooks running on their thread. Dropping this removes them.
pub struct HookThread {
    thread: u32,
}

impl HookThread {
    pub fn thread(&self) -> u32 {
        self.thread
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        stop(self.thread);
    }
}

/// Ask a hook thread to take its hooks out and exit. Harmless if it already has.
pub fn stop(thread: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// Install `hooks` on a new thread with its own message loop - low-level hooks are called there.
/// `ready` runs on that thread once every hook is in, before anyone is told they are.
///
/// Returns `None` if they could not all be installed within `wait`, and then guarantees none of
/// them is left running.
pub fn start(
    hooks: Vec<(WINDOWS_HOOK_ID, HOOKPROC)>,
    wait: Duration,
    ready: fn(),
) -> Option<HookThread> {
    let shared = Arc::new((Mutex::new(Handshake::Waiting), Condvar::new()));
    let theirs = shared.clone();
    std::thread::spawn(move || unsafe {
        // A message queue must exist before anyone can post WM_QUIT to this thread.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let mut installed = Vec::new();
        let mut complete = true;
        for (id, procedure) in hooks {
            match SetWindowsHookExW(id, procedure, None, 0) {
                Ok(h) => installed.push(h),
                Err(_) => {
                    complete = false;
                    break;
                }
            }
        }
        {
            let (state, signal) = &*theirs;
            let mut state = state.lock();
            if !complete || matches!(*state, Handshake::Abandoned) {
                for h in installed {
                    let _ = UnhookWindowsHookEx(h);
                }
                if matches!(*state, Handshake::Waiting) {
                    *state = Handshake::Failed;
                    signal.notify_all();
                }
                return;
            }
            ready();
            *state = Handshake::Running(GetCurrentThreadId());
            signal.notify_all();
        }
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        for h in installed {
            let _ = UnhookWindowsHookEx(h);
        }
    });

    let (state, signal) = &*shared;
    let mut state = state.lock();
    let deadline = Instant::now() + wait;
    while matches!(*state, Handshake::Waiting) {
        if signal.wait_until(&mut state, deadline).timed_out() {
            break;
        }
    }
    match *state {
        Handshake::Running(thread) => Some(HookThread { thread }),
        _ => {
            *state = Handshake::Abandoned;
            None
        }
    }
}
