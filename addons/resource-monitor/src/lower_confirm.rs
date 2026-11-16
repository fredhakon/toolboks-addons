//! The confirm step before "Lower priority" on the Processes page.
//!
//! Lowering a priority lasts until the process exits or the user sets it back, so the button asks
//! first. The state and the wording live here as plain functions so they can be tested without a
//! window; the page only draws the two buttons.

/// A "Lower priority" question waiting for an answer. `T` is the process identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pending<T> {
    /// The processes the question was asked about, in a fixed order.
    ids: Vec<T>,
    /// True until the buttons have been drawn once: Cancel takes the keyboard focus then, so
    /// pressing Enter right away backs out instead of changing anything.
    focus_cancel: bool,
}

impl<T: PartialEq> Pending<T> {
    /// Starts a question about `ids`; nothing to ask when the list is empty.
    pub(super) fn ask(ids: Vec<T>) -> Option<Self> {
        (!ids.is_empty()).then_some(Self {
            ids,
            focus_cancel: true,
        })
    }

    /// True the first time it is called after `ask`, so the page focuses Cancel exactly once.
    pub(super) fn take_cancel_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_cancel)
    }

    fn is_for(&self, current: &[T]) -> bool {
        self.ids == current
    }
}

/// Keeps the question only while it is still about the current selection. A changed selection,
/// a selection with nothing to lower, or a paused list drops it, so an answer can never apply to
/// processes the user did not see named.
pub(super) fn settle<T: PartialEq>(
    pending: Option<Pending<T>>,
    current: &[T],
    allowed: bool,
) -> Option<Pending<T>> {
    pending.filter(|p| allowed && !current.is_empty() && p.is_for(current))
}

/// True when the "Lower priority" answer may go ahead: a question was open and it was about
/// exactly these processes.
pub(super) fn is_confirmed<T: PartialEq>(pending: Option<&Pending<T>>, current: &[T]) -> bool {
    pending.is_some_and(|p| p.is_for(current))
}

/// The question: `label` is an app name or a short description, `count` how many processes.
pub(super) fn lower_text(label: &str, count: usize) -> String {
    if count <= 1 {
        format!(
            "Lower the priority of {label}? It stays lowered until it exits or you set it back."
        )
    } else {
        format!(
            "Lower the priority of {count} processes of {label}? They stay lowered until they exit or you set them back."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_ask_about() {
        assert!(Pending::<u32>::ask(vec![]).is_none());
    }

    #[test]
    fn cancel_gets_focus_once() {
        let mut p = Pending::ask(vec![1u32]).unwrap();
        assert!(p.take_cancel_focus());
        assert!(!p.take_cancel_focus());
    }

    #[test]
    fn question_survives_only_the_same_selection() {
        let p = Pending::ask(vec![1u32, 2]).unwrap();
        assert!(settle(Some(p.clone()), &[1, 2], true).is_some());
        assert!(settle(Some(p.clone()), &[1], true).is_none());
        assert!(settle(Some(p.clone()), &[1, 3], true).is_none());
        assert!(settle(Some(p.clone()), &[], true).is_none());
        assert!(settle(Some(p), &[1, 2], false).is_none());
        assert!(settle::<u32>(None, &[1], true).is_none());
    }

    #[test]
    fn answer_needs_a_matching_question() {
        let p = Pending::ask(vec![7u32]).unwrap();
        assert!(is_confirmed(Some(&p), &[7]));
        assert!(!is_confirmed(Some(&p), &[8]));
        assert!(!is_confirmed(Some(&p), &[]));
        assert!(!is_confirmed::<u32>(None, &[7]));
    }

    #[test]
    fn wording_names_the_process_and_says_how_long() {
        let one = lower_text("Notepad", 1);
        assert_eq!(
            one,
            "Lower the priority of Notepad? It stays lowered until it exits or you set it back."
        );
        let many = lower_text("Chrome", 14);
        assert!(many.contains("14 processes of Chrome"));
        assert!(many.contains("until they exit or you set them back"));
    }
}
