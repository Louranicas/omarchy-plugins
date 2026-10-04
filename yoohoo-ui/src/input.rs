//! Physical edges and displayed-state commands. Never synthesize intent from release.
use std::collections::BTreeSet;
use yoohoo_runtime::{Command, View};

#[derive(Default)]
pub struct Keys {
    held: BTreeSet<u32>,
    exhausted: bool,
}
impl Keys {
    /// Call for every press, including inactive/modifier-rejected input.
    pub fn press(&mut self, code: u32) -> bool {
        if self.exhausted || self.held.contains(&code) {
            return false;
        }
        if self.held.len() == 256 {
            self.exhausted = true;
            return false;
        }
        self.held.insert(code)
    }
    pub fn release(&mut self, code: u32) {
        self.held.remove(&code);
    }
}

#[derive(Clone)]
pub struct DisplayedCommand {
    pub displayed: View,
    pub command: Command,
}
impl DisplayedCommand {
    pub fn capture(view: &View, command: Command) -> Option<Self> {
        (view.open && !view.stale).then(|| Self {
            displayed: view.clone(),
            command,
        })
    }
    pub fn matches(&self, current: &View) -> bool {
        current.open && !current.stale && &self.displayed == current
    }
}
#[derive(Clone, Copy)]
pub enum Action {
    Next,
    Previous,
    Activate,
    Refresh,
    Close,
}
pub fn command(view: &View, action: Action, selected: Option<u64>) -> Option<DisplayedCommand> {
    let generation = view.generation;
    let revision = view.revision;
    let command = match action {
        Action::Next if !view.rows.is_empty() => Command::MoveSelection {
            generation,
            revision,
            next: true,
        },
        Action::Previous if !view.rows.is_empty() => Command::MoveSelection {
            generation,
            revision,
            next: false,
        },
        Action::Activate => Command::Activate {
            generation,
            revision,
            id: selected.filter(|id| view.rows.iter().any(|r| r.id == *id))?,
        },
        Action::Refresh => Command::List,
        Action::Close => Command::Close { generation },
        _ => return None,
    };
    DisplayedCommand::capture(view, command)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn opened() -> yoohoo_runtime::Runtime {
        let mut runtime = yoohoo_runtime::Runtime::fixture().unwrap();
        runtime.execute(Command::Open, 1).unwrap();
        runtime
    }
    #[test]
    fn held_before_open_modifier_and_alias_release_cannot_rearm() {
        let mut keys = Keys::default();
        assert!(keys.press(36)); // Before map or Ctrl+Return: rejected, still latched.
        let runtime = opened();
        assert!(!runtime.view().pending_activation);
        assert!(keys.press(104)); // A different physical key bearing same Return symbol.
        keys.release(104);
        assert!(!keys.press(36)); // Neither alias release nor focus/symbol change resets it.
        keys.release(36);
        assert!(keys.press(36));
        assert!(!keys.press(36));
    }
    #[test]
    fn explicit_enter_after_open_is_distinct_from_external_cycle_accept() {
        let mut runtime = opened();
        let view = runtime.view();
        assert!(
            runtime
                .execute(
                    Command::AcceptCycle {
                        generation: view.generation,
                        revision: view.revision
                    },
                    2
                )
                .is_err()
        );
        assert!(!runtime.view().pending_activation);
        let id = view.rows[0].id;
        let input = command(&view, Action::Activate, Some(id)).unwrap();
        assert!(input.matches(&runtime.view()));
        runtime.execute(input.command, 3).unwrap();
        assert!(runtime.view().pending_activation);
    }
    #[test]
    fn local_arrows_never_authorize_external_cycle_acceptance() {
        let mut runtime = opened();
        let view = runtime.view();
        let next = command(&view, Action::Next, None).unwrap();
        runtime.execute(next.command, 2).unwrap();
        let moved = runtime.view();
        assert_ne!(view.rows, moved.rows);
        assert!(
            runtime
                .execute(
                    Command::AcceptCycle {
                        generation: moved.generation,
                        revision: moved.revision
                    },
                    3
                )
                .is_err()
        );
        assert!(!runtime.view().pending_activation);
        runtime
            .execute(
                Command::Next {
                    generation: moved.generation,
                },
                4,
            )
            .unwrap();
        runtime
            .execute(
                Command::AcceptCycle {
                    generation: moved.generation,
                    revision: moved.revision,
                },
                5,
            )
            .unwrap();
        assert!(runtime.view().pending_activation);
    }
    #[test]
    fn stale_session_selection_empty_and_closed_views_are_rejected() {
        let mut runtime = opened();
        let view = runtime.view();
        let input = command(&view, Action::Activate, Some(view.rows[0].id)).unwrap();
        runtime
            .execute(
                Command::Next {
                    generation: view.generation,
                },
                2,
            )
            .unwrap();
        assert!(!input.matches(&runtime.view())); // Selection changes need not increment attention revision.
        runtime
            .execute(
                Command::Close {
                    generation: view.generation,
                },
                3,
            )
            .unwrap();
        assert!(!input.matches(&runtime.view()));
        runtime.execute(Command::Open, 4).unwrap();
        assert!(!input.matches(&runtime.view()));
        let mut empty = runtime.view();
        empty.rows.clear();
        assert!(command(&empty, Action::Activate, None).is_none());
        assert!(command(&empty, Action::Next, None).is_none());
        empty.stale = true;
        assert!(command(&empty, Action::Close, None).is_none());
    }
    #[test]
    fn overflow_is_sticky_and_releasing_another_held_key_is_independent() {
        let mut keys = Keys::default();
        for code in 0..256 {
            assert!(keys.press(code));
        }
        assert!(!keys.press(999));
        for code in 0..256 {
            keys.release(code);
        }
        assert!(!keys.press(999));
    }
}
