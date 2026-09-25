//! When the destination is decided.
//!
//! **The pinned destination is the field that was focused at the instant the shortcut arrived.**
//! Not the field focused once the microphone finished opening, and not the field focused when a
//! background thread got round to asking. Those are different fields whenever the user clicks
//! away quickly, which is exactly the workflow this product exists for: start dictating, then go
//! and look at something else.
//!
//! The rule is enforced by shape rather than by discipline. [`PendingPin::capture`] takes the
//! snapshot immediately and stores it; [`PendingPin::resolve`] can only ever see what was
//! captured, because it is never handed the focus source. Expensive validation — reading roles,
//! resolving bundle identifiers, asking a browser extension — happens in `resolve`, off the
//! critical path, and cannot accidentally re-read focus.
//!
//! If the captured candidate cannot be validated, the result is a refusal. There is deliberately
//! no path that falls back to whatever is focused now.

use std::fmt::Debug;

/// Somewhere a currently-focused target can be read from.
pub trait FocusSource {
    /// A cheap, immediately-obtainable handle on the focused thing.
    ///
    /// Cheap matters: this runs on the keypress path, ahead of the indicator being drawn.
    /// On macOS it is a retained `AXUIElementRef`, which costs microseconds.
    type Handle: Clone + Debug;

    fn focused_now(&self) -> Option<Self::Handle>;
}

/// A destination candidate, captured at the moment of the shortcut and not yet validated.
#[derive(Debug, Clone)]
pub struct PendingPin<H: Clone + Debug> {
    captured: Option<H>,
}

impl<H: Clone + Debug> PendingPin<H> {
    /// Take the snapshot. Call this first, before anything slow.
    pub fn capture<S: FocusSource<Handle = H>>(source: &S) -> Self {
        PendingPin { captured: source.focused_now() }
    }

    /// For tests and for callers that already hold a handle.
    pub fn from_handle(handle: Option<H>) -> Self {
        PendingPin { captured: handle }
    }

    pub fn candidate(&self) -> Option<&H> {
        self.captured.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.captured.is_none()
    }

    /// Validate the captured candidate, whenever convenient.
    ///
    /// `validate` receives **only** the captured handle. It cannot consult live focus, so a
    /// candidate that fails validation produces an error rather than a different field.
    pub fn resolve<D, E, F>(self, validate: F) -> Result<D, PinResolution<E>>
    where
        F: FnOnce(H) -> Result<D, E>,
    {
        match self.captured {
            None => Err(PinResolution::NothingWasFocused),
            Some(h) => validate(h).map_err(PinResolution::Rejected),
        }
    }
}

/// Why a captured candidate did not become a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinResolution<E> {
    /// Nothing had focus when the shortcut was pressed.
    NothingWasFocused,
    /// Something was captured but could not be validated. **Fail closed** — never substitute.
    Rejected(E),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A focus source that can change between capture and resolve, like a real user clicking.
    struct MovingFocus {
        current: RefCell<Option<&'static str>>,
        reads: RefCell<usize>,
    }
    impl MovingFocus {
        fn on(field: &'static str) -> Self {
            MovingFocus { current: RefCell::new(Some(field)), reads: RefCell::new(0) }
        }
        fn user_clicks_into(&self, field: &'static str) {
            *self.current.borrow_mut() = Some(field);
        }
        fn reads(&self) -> usize {
            *self.reads.borrow()
        }
    }
    impl FocusSource for MovingFocus {
        type Handle = &'static str;
        fn focused_now(&self) -> Option<&'static str> {
            *self.reads.borrow_mut() += 1;
            *self.current.borrow()
        }
    }

    #[test]
    fn the_pin_is_the_field_focused_when_the_shortcut_arrived() {
        // THE RACE. Shortcut pressed in Field A; the user immediately clicks into Field B while
        // the microphone is still opening. The pin must still be Field A.
        let focus = MovingFocus::on("Field A");

        let pending = PendingPin::capture(&focus); // t = 0, the keypress
        focus.user_clicks_into("Field B"); // the user moves on
        let resolved: Result<String, PinResolution<()>> =
            pending.resolve(|h| Ok(format!("pinned:{h}")));

        assert_eq!(resolved.unwrap(), "pinned:Field A");
    }

    #[test]
    fn resolving_never_re_reads_live_focus() {
        // The structural guarantee: `resolve` is not given the source, so it cannot ask again.
        let focus = MovingFocus::on("Field A");
        let pending = PendingPin::capture(&focus);
        assert_eq!(focus.reads(), 1, "focus is read exactly once, at capture");

        focus.user_clicks_into("Field B");
        let _: Result<&str, PinResolution<()>> = pending.resolve(Ok);

        assert_eq!(focus.reads(), 1, "resolve must not consult focus again");
    }

    #[test]
    fn a_candidate_that_fails_validation_fails_closed() {
        // Field A was captured but turns out to be unusable - a password box, a dead reference,
        // an app with no safe write path. The answer is a refusal, never Field B.
        let focus = MovingFocus::on("Field A");
        let pending = PendingPin::capture(&focus);
        focus.user_clicks_into("Field B");

        let resolved: Result<String, PinResolution<&str>> =
            pending.resolve(|_| Err("secure-field"));

        assert_eq!(resolved, Err(PinResolution::Rejected("secure-field")));
    }

    #[test]
    fn nothing_focused_at_the_keypress_is_its_own_outcome() {
        struct Empty;
        impl FocusSource for Empty {
            type Handle = &'static str;
            fn focused_now(&self) -> Option<&'static str> { None }
        }
        let pending = PendingPin::capture(&Empty);
        assert!(pending.is_empty());
        let resolved: Result<&str, PinResolution<()>> = pending.resolve(Ok);
        assert_eq!(resolved, Err(PinResolution::NothingWasFocused));
    }

    #[test]
    fn the_captured_candidate_is_inspectable_before_validation() {
        // The UI shows "→ TextEdit" as soon as something is captured, before the slower
        // validation finishes, so the indicator is never blank while work is happening.
        let focus = MovingFocus::on("Field A");
        let pending = PendingPin::capture(&focus);
        assert_eq!(pending.candidate(), Some(&"Field A"));
    }
}
