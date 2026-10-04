//! Exit decisions and copy ordering, without app, filesystem or clipboard access. The same on
//! macOS and Windows (until 2026-10-03 this was `mac_exit`, and Windows simply left).

use parking_lot::{Mutex, MutexGuard};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QuitDecision {
    Exit,
    Warn,
    Stay,
}

pub(crate) fn quit_decision(
    kept: bool,
    unsaved: bool,
    warned_generation: Option<u64>,
    current_generation: u64,
) -> QuitDecision {
    if !kept {
        QuitDecision::Stay
    } else if !unsaved || warned_generation == Some(current_generation) {
        QuitDecision::Exit
    } else {
        QuitDecision::Warn
    }
}

/// The early copy holds a guard through its snapshot and write. The final copy marks itself
/// started under the same lock, then releases it before doing any delivery work.
pub(crate) struct CopyOrder<'a> {
    final_started: &'a Mutex<bool>,
}

impl<'a> CopyOrder<'a> {
    pub(crate) fn new(final_started: &'a Mutex<bool>) -> Self {
        Self { final_started }
    }

    pub(crate) fn early(&self) -> Option<MutexGuard<'a, bool>> {
        let guard = self.final_started.lock();
        if *guard { None } else { Some(guard) }
    }

    pub(crate) fn start_final(&self) {
        *self.final_started.lock() = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn unsaved_dictation_warns_once_then_allows_quit() {
        assert_eq!(quit_decision(true, true, None, 1), QuitDecision::Warn);
        assert_eq!(quit_decision(true, true, Some(1), 1), QuitDecision::Exit);
    }

    #[test]
    fn a_new_dictation_cannot_reuse_an_older_warning() {
        // A was warned at generation 1; dismiss and start each bump the generation.
        assert_eq!(quit_decision(true, true, Some(1), 3), QuitDecision::Warn);
        assert_eq!(quit_decision(true, true, Some(3), 3), QuitDecision::Exit);
    }

    #[test]
    fn a_timeout_stays_and_the_next_quit_still_warns() {
        let warned = None; // Stay does not record a warning.
        assert_eq!(quit_decision(false, true, warned, 1), QuitDecision::Stay);
        assert_eq!(quit_decision(true, true, warned, 1), QuitDecision::Warn);
        assert_eq!(quit_decision(false, true, Some(1), 1), QuitDecision::Stay);
    }

    #[test]
    fn nothing_unsaved_after_dismiss_needs_no_warning() {
        assert_eq!(quit_decision(true, false, Some(1), 2), QuitDecision::Exit);
        assert_eq!(quit_decision(true, false, None, 2), QuitDecision::Exit);
    }

    #[test]
    fn final_copy_waits_for_early_copy_and_writes_last() {
        let started = Arc::new(Mutex::new(false));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let early = {
            let started = started.clone();
            let writes = writes.clone();
            std::thread::spawn(move || {
                let order = CopyOrder::new(&started);
                let _guard = order.early().expect("the final copy has not begun");
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                writes.lock().push("partial");
            })
        };
        held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (trying_tx, trying_rx) = mpsc::channel();
        let (begun_tx, begun_rx) = mpsc::channel();
        let final_copy = {
            let started = started.clone();
            let writes = writes.clone();
            std::thread::spawn(move || {
                trying_tx.send(()).unwrap();
                CopyOrder::new(&started).start_final();
                begun_tx.send(()).unwrap();
                writes.lock().push("full");
            })
        };
        trying_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(started.try_lock().is_none(), "the early copy retains the lock");
        assert_eq!(begun_rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout));
        release_tx.send(()).unwrap();
        early.join().unwrap();
        final_copy.join().unwrap();
        assert_eq!(*writes.lock(), vec!["partial", "full"]);
    }

    #[test]
    fn early_copy_is_skipped_once_final_copy_has_begun() {
        let started = Arc::new(Mutex::new(false));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (begun_tx, begun_rx) = mpsc::channel();
        let (checked_tx, checked_rx) = mpsc::channel();
        let final_copy = {
            let started = started.clone();
            let writes = writes.clone();
            std::thread::spawn(move || {
                CopyOrder::new(&started).start_final();
                begun_tx.send(()).unwrap();
                checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                writes.lock().push("full");
            })
        };
        let early = {
            let started = started.clone();
            let writes = writes.clone();
            std::thread::spawn(move || {
                begun_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let order = CopyOrder::new(&started);
                let guard = order.early();
                assert!(guard.is_none(), "even before the final write, no early copy may run");
                if guard.is_some() {
                    writes.lock().push("partial");
                }
                checked_tx.send(()).unwrap();
            })
        };
        early.join().unwrap();
        final_copy.join().unwrap();
        assert_eq!(*writes.lock(), vec!["full"]);
    }
}
