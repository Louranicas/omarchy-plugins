//! Pure layout action/config resolution. Plans require fresh compositor identity
//! and fenced execution by an external adapter; these enums never dispatch Lua.
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DoubleAction {
    ToggleMaximized,
    PromoteMaster,
    Disabled,
    Command(Vec<String>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldAction {
    Pair,
    Place,
    Group,
    Disabled,
    Command(Vec<String>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidLayout,
    InvalidCommand,
    InvalidGeometry,
    SameGroup,
}
fn valid_layout(layout: &str) -> Result<(), Error> {
    if layout.is_empty() || layout.len() > 128 || layout.chars().any(char::is_control) {
        Err(Error::InvalidLayout)
    } else {
        Ok(())
    }
}
pub fn validate_command(argv: &[String]) -> Result<(), Error> {
    if argv.is_empty()
        || argv.len() > 64
        || argv[0].is_empty()
        || argv.iter().any(|a| a.len() > 4096 || a.contains('\0'))
    {
        Err(Error::InvalidCommand)
    } else {
        Ok(())
    }
}
pub fn double_action(
    layout: &str,
    alt: bool,
    normal: &BTreeMap<String, DoubleAction>,
    alternate: &BTreeMap<String, DoubleAction>,
) -> Result<DoubleAction, Error> {
    valid_layout(layout)?;
    let map = if alt { alternate } else { normal };
    let action = map
        .get(layout)
        .or_else(|| map.get("*"))
        .cloned()
        .unwrap_or_else(|| {
            if alt || matches!(layout, "dwindle" | "scrolling") {
                DoubleAction::ToggleMaximized
            } else if layout == "master" {
                DoubleAction::PromoteMaster
            } else {
                DoubleAction::Disabled
            }
        });
    if let DoubleAction::Command(ref argv) = action {
        validate_command(argv)?;
    }
    Ok(action)
}
pub fn hold_action(
    layout: &str,
    cross_monitor: bool,
    configured: &BTreeMap<String, HoldAction>,
) -> Result<HoldAction, Error> {
    valid_layout(layout)?;
    // Legacy cross-monitor placement intentionally wins over configured callbacks.
    let action = if cross_monitor {
        if layout == "dwindle" {
            HoldAction::Pair
        } else {
            HoldAction::Place
        }
    } else {
        configured
            .get(layout)
            .or_else(|| configured.get("*"))
            .cloned()
            .unwrap_or(if layout == "dwindle" {
                HoldAction::Pair
            } else {
                HoldAction::Disabled
            })
    };
    if let HoldAction::Command(ref argv) = action {
        validate_command(argv)?;
    }
    Ok(action)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}
/// Source-relative split direction; ties use the horizontal axis, including
/// coincident centers (Right), matching the source's dx² >= dy² branch.
pub fn direction(source: [f64; 2], target: [f64; 2]) -> Result<Direction, Error> {
    if !source.iter().chain(&target).all(|x| x.is_finite()) {
        return Err(Error::InvalidGeometry);
    }
    let dx = source[0] - target[0];
    let dy = source[1] - target[1];
    if !dx.is_finite() || !dy.is_finite() {
        return Err(Error::InvalidGeometry);
    }
    Ok(if dx.abs() >= dy.abs() {
        if dx < 0.0 {
            Direction::Left
        } else {
            Direction::Right
        }
    } else if dy < 0.0 {
        Direction::Up
    } else {
        Direction::Down
    })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    DwindleSibling(Direction),
    ScrollingRight,
    MasterPromote,
    MasterAfterTarget,
    Unsupported,
}
pub fn placement(
    layout: &str,
    source: [f64; 2],
    target: [f64; 2],
    target_is_master: bool,
) -> Result<Placement, Error> {
    valid_layout(layout)?;
    Ok(match layout {
        "dwindle" => Placement::DwindleSibling(direction(source, target)?),
        "scrolling" => Placement::ScrollingRight,
        "master" if target_is_master => Placement::MasterPromote,
        "master" => Placement::MasterAfterTarget,
        _ => Placement::Unsupported,
    })
}
/// Exact legacy callback variable names. Values are argv/environment operands;
/// adapters must validate addresses/identities before invoking the callback.
pub fn double_environment(
    layout: &str,
    address: &str,
    stable_id: &str,
    workspace: i64,
    fullscreen: u8,
    hint: &str,
) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("VIMARCHY_LAYOUT", layout.into()),
        ("VIMARCHY_WINDOW_ADDRESS", address.into()),
        ("VIMARCHY_WINDOW_STABLE_ID", stable_id.into()),
        ("VIMARCHY_WORKSPACE_ID", workspace.to_string()),
        ("VIMARCHY_FULLSCREEN_STATE", fullscreen.to_string()),
        ("VIMARCHY_HINT", hint.into()),
    ])
}
#[derive(Debug)]
pub struct CallbackTarget<'a> {
    pub address: &'a str,
    pub stable_id: &'a str,
    pub hint: &'a str,
}
pub fn hold_environment(
    layout: &str,
    source: CallbackTarget<'_>,
    target: CallbackTarget<'_>,
    workspace: i64,
) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("VIMARCHY_LAYOUT", layout.into()),
        ("VIMARCHY_SOURCE_ADDRESS", source.address.into()),
        ("VIMARCHY_SOURCE_STABLE_ID", source.stable_id.into()),
        ("VIMARCHY_TARGET_ADDRESS", target.address.into()),
        ("VIMARCHY_TARGET_STABLE_ID", target.stable_id.into()),
        ("VIMARCHY_WORKSPACE_ID", workspace.to_string()),
        ("VIMARCHY_SOURCE_HINT", source.hint.into()),
        ("VIMARCHY_TARGET_HINT", target.hint.into()),
    ])
}
/// Membership intent preserves the complete source group. The executor must
/// compare these exact expected memberships to fresh snapshots before each
/// operation, then verify final membership; it must not execute stale addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupPlan {
    pub moving: Vec<String>,
    pub expected_target: Vec<String>,
    pub expected_merged: Vec<String>,
}
pub fn group_plan(source: &[String], target: &[String]) -> Result<GroupPlan, Error> {
    use std::collections::BTreeSet;
    let valid = |ids: &[String]| {
        !ids.is_empty()
            && ids.len() <= crate::allocation::CAPACITY
            && ids
                .iter()
                .all(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
    };
    if !valid(source) || !valid(target) {
        return Err(Error::InvalidCommand);
    }
    let source_set: BTreeSet<_> = source.iter().collect();
    let target_set: BTreeSet<_> = target.iter().collect();
    if source_set.len() != source.len() || target_set.len() != target.len() {
        return Err(Error::InvalidCommand);
    }
    if source_set.intersection(&target_set).next().is_some() {
        return Err(Error::SameGroup);
    }
    if source.len() + target.len() > crate::allocation::CAPACITY {
        return Err(Error::InvalidCommand);
    }
    let mut merged = target.to_vec();
    merged.extend_from_slice(source);
    Ok(GroupPlan {
        moving: source.to_vec(),
        expected_target: target.to_vec(),
        expected_merged: merged,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_defaults_and_alternate() {
        for (layout, expected) in [
            ("dwindle", DoubleAction::ToggleMaximized),
            ("scrolling", DoubleAction::ToggleMaximized),
            ("master", DoubleAction::PromoteMaster),
            ("other", DoubleAction::Disabled),
        ] {
            assert_eq!(
                double_action(layout, false, &BTreeMap::new(), &BTreeMap::new()).unwrap(),
                expected
            );
            assert_eq!(
                double_action(layout, true, &BTreeMap::new(), &BTreeMap::new()).unwrap(),
                DoubleAction::ToggleMaximized
            );
        }
    }
    #[test]
    fn per_layout_before_wildcard_and_alt_independent() {
        let map = BTreeMap::from([
            ("*".into(), DoubleAction::Disabled),
            ("master".into(), DoubleAction::ToggleMaximized),
        ]);
        assert_eq!(
            double_action("master", false, &map, &BTreeMap::new()).unwrap(),
            DoubleAction::ToggleMaximized
        );
        assert_eq!(
            double_action("dwindle", false, &map, &BTreeMap::new()).unwrap(),
            DoubleAction::Disabled
        );
        assert_eq!(
            double_action("dwindle", true, &map, &BTreeMap::new()).unwrap(),
            DoubleAction::ToggleMaximized
        );
    }
    #[test]
    fn cross_monitor_overrides_disabled_and_commands() {
        let map = BTreeMap::from([("*".into(), HoldAction::Disabled)]);
        assert_eq!(
            hold_action("master", false, &map).unwrap(),
            HoldAction::Disabled
        );
        assert_eq!(
            hold_action("master", true, &map).unwrap(),
            HoldAction::Place
        );
        assert_eq!(
            hold_action("dwindle", true, &map).unwrap(),
            HoldAction::Pair
        );
        assert_eq!(
            hold_action("dwindle", false, &BTreeMap::new()).unwrap(),
            HoldAction::Pair
        );
    }
    #[test]
    fn geometry_ties_and_finite_bounds() {
        assert_eq!(direction([0., 0.], [0., 0.]), Ok(Direction::Right));
        assert_eq!(direction([-2., 2.], [0., 0.]), Ok(Direction::Left));
        assert_eq!(direction([0., -3.], [0., 0.]), Ok(Direction::Up));
        assert_eq!(
            direction([f64::MAX, 0.], [-f64::MAX, 0.]),
            Err(Error::InvalidGeometry)
        );
        assert_eq!(
            placement("master", [0., 0.], [0., 0.], true),
            Ok(Placement::MasterPromote)
        );
        assert_eq!(
            placement("scrolling", [0., 0.], [0., 0.], false),
            Ok(Placement::ScrollingRight)
        );
    }
    #[test]
    fn exact_command_and_environment() {
        let argv = vec!["/bin/program".into(), "line\n'\"$()-dash".into()];
        assert_eq!(validate_command(&argv), Ok(()));
        assert_eq!(validate_command(&["".into()]), Err(Error::InvalidCommand));
        assert_eq!(
            validate_command(&["a\0b".into()]),
            Err(Error::InvalidCommand)
        );
        let env = double_environment("master", "0xa", "a", -99, 1, "aa");
        assert_eq!(env.len(), 6);
        assert_eq!(env["VIMARCHY_HINT"], "aa");
        let env = hold_environment(
            "dwindle",
            CallbackTarget {
                address: "0xa",
                stable_id: "a",
                hint: "a",
            },
            CallbackTarget {
                address: "0xb",
                stable_id: "b",
                hint: "s",
            },
            2,
        );
        assert_eq!(env.len(), 8);
        assert_eq!(env["VIMARCHY_TARGET_HINT"], "s");
    }
    #[test]
    fn whole_source_group_membership_is_preserved_and_overlap_rejected() {
        let source = vec!["a".into(), "b".into()];
        let target = vec!["c".into(), "d".into()];
        let plan = group_plan(&source, &target).unwrap();
        assert_eq!(plan.moving, source);
        assert_eq!(plan.expected_merged, vec!["c", "d", "a", "b"]);
        assert_eq!(group_plan(&source, &["b".into()]), Err(Error::SameGroup));
        assert!(group_plan(&["a".into(), "a".into()], &target).is_err());
    }
}
