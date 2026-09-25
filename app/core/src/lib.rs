//! Huck's Voice to Text — platform-free core.
//!
//! Everything in this crate must compile and pass its tests on any OS. The only genuinely
//! OS-native parts of the product — field capture and text injection — are behind the
//! [`pipeline::Destination`] trait and live in the desktop crate's platform layer.
//!
//! The product rule this crate exists to enforce: **never lose the user's dictated text.**
//! That rule is encoded in [`pipeline::complete_transcription`], which persists a recovery
//! draft and copies to the clipboard *before* any delivery is attempted, and which has no
//! code path that discards a transcript.

pub mod audio;
pub mod drafts;
pub mod engine;
pub mod paths;
pub mod pinning;
pub mod pipeline;
pub mod session;
pub mod settings;
pub mod transcript;

pub use pipeline::{complete_transcription, CompletionReport, DeliveryOutcome};
pub use session::{SessionState, StateError};
pub use transcript::Transcript;
