//! The transcript model.
//!
//! Shaped for a two-pass design: a fast model paints a *provisional* draft while the user speaks,
//! an accurate model replaces it with *settled*
//! text, and delivery always uses the settled pass. The first milestone only produces settled
//! text, but the distinction is in the type from the start so the UI never has to be rebuilt
//! around it — and so provisional text can be rendered visually distinct, which the doctrine
//! requires.
//!
//! The invariant that matters most: **a user edit is never overwritten by a later recognition
//! pass.** Text the user has touched is theirs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// A fast pass. May still change underneath the user.
    Provisional,
    /// An accurate pass. Safe to deliver.
    Settled,
    /// The user typed this. Never replaced by recognition.
    Edited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    text: String,
    provenance: Provenance,
}

impl Transcript {
    pub fn empty() -> Self {
        Transcript { text: String::new(), provenance: Provenance::Settled }
    }

    pub fn settled(text: impl Into<String>) -> Self {
        Transcript { text: text.into(), provenance: Provenance::Settled }
    }

    pub fn provisional(text: impl Into<String>) -> Self {
        Transcript { text: text.into(), provenance: Provenance::Provisional }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// True when this text is safe to deliver to a destination.
    pub fn is_deliverable(&self) -> bool {
        !self.is_empty() && self.provenance != Provenance::Provisional
    }

    /// The user typed in the composer. This latches: recognition can no longer overwrite it.
    pub fn edit(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.provenance = Provenance::Edited;
    }

    /// A recognition pass produced new text.
    ///
    /// Returns `false` and changes nothing when the user has already edited — losing someone's
    /// correction to a late-arriving pass is exactly the "changes underneath him" failure the
    /// doctrine forbids.
    pub fn apply_recognition(&mut self, text: impl Into<String>, provenance: Provenance) -> bool {
        if self.provenance == Provenance::Edited {
            return false;
        }
        // A provisional pass must never clobber settled text.
        if self.provenance == Provenance::Settled && provenance == Provenance::Provisional {
            return false;
        }
        self.text = text.into();
        self.provenance = provenance;
        true
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Transcript::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_text_is_deliverable_and_provisional_is_not() {
        assert!(Transcript::settled("hello there").is_deliverable());
        assert!(!Transcript::provisional("hello there").is_deliverable());
        assert!(!Transcript::settled("   ").is_deliverable());
    }

    #[test]
    fn accurate_pass_replaces_provisional_draft() {
        let mut t = Transcript::provisional("helo ther");
        assert!(t.apply_recognition("hello there", Provenance::Settled));
        assert_eq!(t.text(), "hello there");
        assert!(t.is_deliverable());
    }

    #[test]
    fn a_user_edit_is_never_overwritten_by_recognition() {
        let mut t = Transcript::provisional("helo ther");
        t.edit("Hello there, Taylor.");
        // The accurate pass lands late — it must not win.
        assert!(!t.apply_recognition("hello there", Provenance::Settled));
        assert_eq!(t.text(), "Hello there, Taylor.");
        assert_eq!(t.provenance(), Provenance::Edited);
    }

    #[test]
    fn a_provisional_pass_cannot_clobber_settled_text() {
        let mut t = Transcript::settled("hello there");
        assert!(!t.apply_recognition("helo", Provenance::Provisional));
        assert_eq!(t.text(), "hello there");
    }

    #[test]
    fn an_edit_is_deliverable() {
        let mut t = Transcript::provisional("x");
        t.edit("corrected text");
        assert!(t.is_deliverable());
    }
}
