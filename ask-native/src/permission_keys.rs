//! Physical decision-key latches survive permission replacement and modifier changes.
use std::collections::BTreeSet;
#[derive(Default)]
pub struct DecisionKeys {
    held: BTreeSet<u32>,
    exhausted: bool,
}
impl DecisionKeys {
    /// Observe every physical press before filtering symbols, modifiers or UI state.
    pub fn press(&mut self, keycode: u32) -> bool {
        if self.exhausted || self.held.contains(&keycode) {
            return false;
        }
        if self.held.len() == 256 {
            self.exhausted = true;
            return false;
        }
        self.held.insert(keycode)
    }
    /// Releasing another key or changing focus never rearms this physical key.
    pub fn release(&mut self, keycode: u32) {
        self.held.remove(&keycode);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typing_key_held_before_permission_cannot_approve_on_repeat() {
        let mut keys = DecisionKeys::default();
        assert!(keys.press(29)); // Typing, no permission existed; no decision emitted.
        for _ in 0..20 {
            assert!(!keys.press(29));
        } // Permission now visible.
        keys.release(29);
        assert!(keys.press(29)); // Only a fresh explicit press may decide.
    }
    #[test]
    fn other_key_release_does_not_rearm_held_allow_for_successor() {
        let mut keys = DecisionKeys::default();
        assert!(keys.press(29)); // First request answered, successor becomes visible.
        assert!(keys.press(57)); // Ctrl+N is filtered by UI, but must still be tracked.
        keys.release(57);
        assert!(!keys.press(29)); // A shared boolean incorrectly accepts this repeat.
        keys.release(29);
        assert!(keys.press(29));
    }
    #[test]
    fn physical_key_survives_symbol_and_focus_changes_and_exhaustion_is_sticky() {
        let mut keys = DecisionKeys::default();
        assert!(keys.press(29)); // Earlier symbol was not Y; focus leaves and returns.
        assert!(!keys.press(29)); // Same physical repeat now labelled Y.
        keys.release(29);
        for code in 0..256 {
            assert!(keys.press(code));
        }
        assert!(!keys.press(999));
        for code in 0..256 {
            keys.release(code);
        }
        assert!(!keys.press(999)); // Unknown held overflow cannot become fresh intent.
    }
    #[test]
    fn modifier_release_and_opposite_key_are_independent() {
        let mut keys = DecisionKeys::default();
        assert!(keys.press(57)); // Ctrl+N initially; Ctrl later released.
        assert!(keys.press(29));
        keys.release(29);
        assert!(!keys.press(57));
        keys.release(57);
        assert!(keys.press(57));
    }
}
