//! The session state machine.
//!
//! The UI shows exactly one of these states at all times, so the user can always tell whether
//! Huck is listening, thinking, or holding finished words. Illegal transitions are rejected
//! rather than silently ignored, because a state machine that quietly accepts nonsense is how
//! a recording ends up running with no indicator.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum SessionState {
    /// Nothing happening. The composer may still hold a previous transcript.
    Idle,
    /// The microphone is open.
    Recording,
    /// Audio captured, recognition running.
    Transcribing,
    /// Words are in the composer, on the clipboard, and waiting for the user.
    Ready,
    /// Something failed. `message` is shown verbatim; the transcript is never cleared.
    Error { message: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateError {
    #[error("cannot go from {from} to {to}")]
    IllegalTransition { from: &'static str, to: &'static str },
}

impl SessionState {
    pub fn name(&self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Recording => "recording",
            SessionState::Transcribing => "transcribing",
            SessionState::Ready => "ready",
            SessionState::Error { .. } => "error",
        }
    }

    /// True while the microphone is open. The tray and composer use this, not string matching.
    pub fn is_capturing(&self) -> bool {
        matches!(self, SessionState::Recording)
    }

    /// True when the hotkey should start a new recording rather than stop one.
    pub fn accepts_start(&self) -> bool {
        matches!(
            self,
            SessionState::Idle | SessionState::Ready | SessionState::Error { .. }
        )
    }

    /// Attempt a transition, rejecting the ones that make no sense.
    ///
    /// An error may be entered from anywhere — failures do not respect a happy path. Every
    /// other move is constrained.
    pub fn transition_to(&self, next: SessionState) -> Result<SessionState, StateError> {
        use SessionState::*;
        let ok = match (self, &next) {
            // Failure can arrive at any moment.
            (_, Error { .. }) => true,
            // Starting over is always allowed from a resting state.
            (s, Recording) if s.accepts_start() => true,
            (Recording, Transcribing) => true,
            (Transcribing, Ready) => true,
            // Dismissing the composer, or abandoning a recording.
            (Ready, Idle) | (Error { .. }, Idle) | (Recording, Idle) | (Idle, Idle) => true,
            _ => false,
        };

        if ok {
            Ok(next)
        } else {
            Err(StateError::IllegalTransition {
                from: self.name(),
                to: next.name(),
            })
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_runs_end_to_end() {
        let s = SessionState::Idle;
        let s = s.transition_to(SessionState::Recording).unwrap();
        let s = s.transition_to(SessionState::Transcribing).unwrap();
        let s = s.transition_to(SessionState::Ready).unwrap();
        assert_eq!(s, SessionState::Ready);
    }

    #[test]
    fn cannot_transcribe_without_recording() {
        let err = SessionState::Idle
            .transition_to(SessionState::Transcribing)
            .unwrap_err();
        assert_eq!(
            err,
            StateError::IllegalTransition { from: "idle", to: "transcribing" }
        );
    }

    #[test]
    fn cannot_jump_straight_to_ready() {
        assert!(SessionState::Recording
            .transition_to(SessionState::Ready)
            .is_err());
    }

    #[test]
    fn error_is_reachable_from_every_state() {
        let err = || SessionState::Error { message: "boom".into() };
        for s in [
            SessionState::Idle,
            SessionState::Recording,
            SessionState::Transcribing,
            SessionState::Ready,
        ] {
            assert!(s.transition_to(err()).is_ok(), "{} should accept error", s.name());
        }
    }

    #[test]
    fn a_new_recording_can_start_after_ready_or_error() {
        assert!(SessionState::Ready.transition_to(SessionState::Recording).is_ok());
        assert!(SessionState::Error { message: "x".into() }
            .transition_to(SessionState::Recording)
            .is_ok());
    }

    #[test]
    fn hotkey_starts_from_resting_states_only() {
        assert!(SessionState::Idle.accepts_start());
        assert!(SessionState::Ready.accepts_start());
        assert!(!SessionState::Recording.accepts_start());
        // Mid-recognition the hotkey must not kick off a second capture.
        assert!(!SessionState::Transcribing.accepts_start());
    }
}
