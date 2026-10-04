use std::collections::{BTreeMap, BTreeSet};

pub const ALPHABET: &str = "asdfghjklqwertyuiopzxcvbnm";
pub const CAPACITY: usize = 676;
pub const MAX_LIVE_IDS: usize = 16_384;
pub const MAX_ID_BYTES: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TooManyLiveIds,
    InvalidIdentity,
    DuplicateVisibleIdentity,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Allocation {
    /// Visible selectable identities only, with collision-free hints.
    pub visible: BTreeMap<String, String>,
    /// Previous assignments for live hidden identities survive, even if their
    /// labels collide with visible ones. They are revalidated on reappearance.
    pub remembered: BTreeMap<String, String>,
    pub omitted: usize,
}

/// Input order is the source snapshot's monitor/y/x order. The first 676
/// identities remain selectable, matching the source capacity contract.
pub fn allocate(
    visible: &[String],
    live: &[String],
    previous: &BTreeMap<String, String>,
) -> Result<Allocation, Error> {
    if live.len() > MAX_LIVE_IDS || visible.len() > MAX_LIVE_IDS {
        return Err(Error::TooManyLiveIds);
    }
    if visible
        .iter()
        .chain(live)
        .any(|id| id.is_empty() || id.len() > MAX_ID_BYTES || id.contains('\0'))
    {
        return Err(Error::InvalidIdentity);
    }
    let mut unique = BTreeSet::new();
    if visible.iter().any(|id| !unique.insert(id)) {
        return Err(Error::DuplicateVisibleIdentity);
    }
    let omitted = visible.len().saturating_sub(CAPACITY);
    let visible = &visible[..visible.len().min(CAPACITY)];
    let alphabet: Vec<char> = ALPHABET.chars().collect();
    let hints: Vec<String> = if visible.len() <= alphabet.len() {
        alphabet.iter().map(char::to_string).collect()
    } else {
        alphabet
            .iter()
            .flat_map(|first| {
                alphabet
                    .iter()
                    .map(move |second| format!("{first}{second}"))
            })
            .collect()
    };
    let valid: BTreeSet<&str> = hints.iter().map(String::as_str).collect();
    let mut used = BTreeSet::new();
    let mut assigned = BTreeMap::new();
    // Preserve all eligible old assignments before allocating new ones. Doing
    // both in one pass could steal a later identity's preserved label.
    for id in visible {
        if let Some(hint) = previous.get(id)
            && valid.contains(hint.as_str())
            && used.insert(hint.as_str())
        {
            assigned.insert(id.clone(), hint.clone());
        }
    }
    let mut remaining = hints.iter().filter(|hint| !used.contains(hint.as_str()));
    for id in visible {
        if !assigned.contains_key(id)
            && let Some(hint) = remaining.next()
        {
            assigned.insert(id.clone(), hint.clone());
        }
    }
    let mut remembered = assigned.clone();
    for id in live.iter().chain(visible) {
        if !remembered.contains_key(id)
            && let Some(hint) = previous.get(id)
            && (1..=2).contains(&hint.len())
            && hint.chars().all(|c| ALPHABET.contains(c))
        {
            remembered.insert(id.clone(), hint.clone());
        }
    }
    Ok(Allocation {
        visible: assigned,
        remembered,
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("window-{i}")).collect()
    }

    #[test]
    fn alphabet_capacity_and_width_transitions() {
        for n in [0, 1, 26, 27, 675, 676, 677] {
            let input = ids(n);
            let result = allocate(&input, &input, &BTreeMap::new()).unwrap();
            assert_eq!(result.visible.len(), n.min(CAPACITY));
            assert_eq!(result.omitted, n.saturating_sub(CAPACITY));
            let distinct: BTreeSet<_> = result.visible.values().collect();
            assert_eq!(distinct.len(), result.visible.len());
            assert!(
                distinct
                    .iter()
                    .all(|h| h.len() == if n <= 26 { 1 } else { 2 })
            );
            if n > 0 {
                assert_eq!(result.visible["window-0"], if n <= 26 { "a" } else { "aa" });
            }
        }
    }

    #[test]
    fn later_preserved_label_cannot_be_stolen() {
        let previous = BTreeMap::from([("old".into(), "a".into())]);
        let result = allocate(&["new".into(), "old".into()], &[], &previous).unwrap();
        assert_eq!(result.visible["old"], "a");
        assert_eq!(result.visible["new"], "s");
    }

    #[test]
    fn hidden_memory_is_pruned_and_revalidated() {
        let previous = BTreeMap::from([
            ("hidden".into(), "a".into()),
            ("dead".into(), "s".into()),
            ("active".into(), "a".into()),
        ]);
        let first = allocate(&["active".into()], &["hidden".into()], &previous).unwrap();
        assert!(!first.remembered.contains_key("dead"));
        assert_eq!(first.remembered["hidden"], "a");
        let next = allocate(&["active".into(), "hidden".into()], &[], &first.remembered).unwrap();
        assert_eq!(next.visible["active"], "a");
        assert_eq!(next.visible["hidden"], "s");
    }

    #[test]
    fn width_changes_invalidate_old_labels() {
        let input = ids(27);
        let previous = BTreeMap::from([("window-0".into(), "s".into())]);
        let result = allocate(&input, &input, &previous).unwrap();
        assert_eq!(result.visible["window-0"], "aa");
        let back = allocate(&input[..1], &input, &result.remembered).unwrap();
        assert_eq!(back.visible["window-0"], "a");
    }

    #[test]
    fn malformed_identity_and_oversized_inputs_fail_before_allocation() {
        assert_eq!(
            allocate(&["x".into(), "x".into()], &[], &BTreeMap::new()),
            Err(Error::DuplicateVisibleIdentity)
        );
        for id in [
            "".to_string(),
            "x\0y".to_string(),
            "x".repeat(MAX_ID_BYTES + 1),
        ] {
            assert_eq!(
                allocate(&[id], &[], &BTreeMap::new()),
                Err(Error::InvalidIdentity)
            );
        }
        assert_eq!(
            allocate(&[], &ids(MAX_LIVE_IDS + 1), &BTreeMap::new()),
            Err(Error::TooManyLiveIds)
        );
    }

    #[test]
    fn input_order_and_replay_are_deterministic() {
        for n in [1, 26, 27, 100, 676] {
            let input = ids(n);
            let first = allocate(&input, &input, &BTreeMap::new()).unwrap();
            let mut reversed = input.clone();
            reversed.reverse();
            let replay = allocate(&reversed, &input, &first.remembered).unwrap();
            assert_eq!(first, replay);
        }
    }
}
