//! Shared, typed grammar for application-level Vim navigation.
//!
//! This module deliberately owns no screen state. Chat and Settings keep
//! their own stable-ID selections and translate these typed GPUI actions into
//! existing feature behavior. The controller/model-driven layer is a later
//! consumer of those screen-owned seams, not a dependency of human Vim.

use gpui::Action;

/// Root application-navigation bindings. The negative child contexts keep
/// ordinary inputs and an open popup menu in charge while they own focus.
pub(crate) const ROOT_CONTEXT: &str = "ApplicationVim && !TextInput && !Menu";
/// Vim aliases while an open popup menu owns focus.
pub(crate) const MENU_CONTEXT: &str = "ApplicationVim && Menu";
/// Escape from an ordinary field returns to its screen's application proxy.
pub(crate) const OTHER_INPUT_CONTEXT: &str = "ApplicationVim && TextInput && input_role == other";
/// Application chords intentionally reserved from composer Normal mode.
pub(crate) const COMPOSER_NORMAL_CONTEXT: &str =
    "ApplicationVim && TextInput && input_role == composer && editor_vim_mode == normal";

gpui::actions!(
    application_vim,
    [
        Activate,
        Collapse,
        CopyTarget,
        Escape,
        Expand,
        First,
        FocusComposer,
        Last,
        NewestAssistant,
        Next,
        NextAnnotation,
        NextAssistant,
        Previous,
        PreviousAnnotation,
        PreviousAssistant,
        Search,
    ]
);

/// One digit of a bounded application-Vim count prefix.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = application_vim, no_json)]
pub(crate) struct CountDigit {
    pub(crate) digit: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SpatialDirection {
    Left,
    Down,
    Up,
    #[default]
    Right,
}

/// Move between semantic screen regions without exposing physical keys to a
/// screen implementation.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = application_vim, no_json)]
pub(crate) struct MoveRegion {
    pub(crate) direction: SpatialDirection,
}

pub(crate) const MAX_COUNT: usize = 999_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountOutcome {
    /// A leading zero is not a count. The surface may assign it another
    /// meaning; Maple currently treats it as a consumed no-op.
    LeadingZero,
    Pending(usize),
    Capped(usize),
}

/// Pure prefix state shared by Chat and Settings. A count is consumed once by
/// the next countable command and every other command clears it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CountState {
    pending: Option<usize>,
    capped: bool,
}

impl CountState {
    pub(crate) fn push(&mut self, digit: u8) -> CountOutcome {
        debug_assert!(digit <= 9);
        if digit == 0 && self.pending.is_none() {
            return CountOutcome::LeadingZero;
        }
        let current = self.pending.unwrap_or(0);
        let next = current
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(digit)));
        match next {
            Some(next) if next <= MAX_COUNT => {
                self.pending = Some(next);
                self.capped = false;
                CountOutcome::Pending(next)
            }
            _ => {
                self.pending = Some(MAX_COUNT);
                self.capped = true;
                CountOutcome::Capped(MAX_COUNT)
            }
        }
    }

    pub(crate) fn take(&mut self) -> usize {
        self.capped = false;
        self.pending.take().unwrap_or(1)
    }

    pub(crate) fn clear(&mut self) -> bool {
        let changed = self.pending.take().is_some() || self.capped;
        self.capped = false;
        changed
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> Option<usize> {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_only_extends_an_existing_count() {
        let mut count = CountState::default();
        assert_eq!(count.push(0), CountOutcome::LeadingZero);
        assert_eq!(count.pending(), None);
        assert_eq!(count.push(2), CountOutcome::Pending(2));
        assert_eq!(count.push(0), CountOutcome::Pending(20));
        assert_eq!(count.take(), 20);
        assert_eq!(count.take(), 1);
    }

    #[test]
    fn count_caps_without_wrapping_and_clear_resets_it() {
        let mut count = CountState::default();
        for _ in 0..6 {
            count.push(9);
        }
        assert_eq!(count.push(9), CountOutcome::Capped(MAX_COUNT));
        assert_eq!(count.pending(), Some(MAX_COUNT));
        assert!(count.clear());
        assert!(!count.clear());
        assert_eq!(count.pending(), None);
    }
}
