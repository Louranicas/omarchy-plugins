//! Read-only proposals from fresh native observations. A proposal is never a
//! reusable effect permit: the arbiter must repeat identity/state/policy checks
//! within its fenced serialized effect handler before implementing submission.
use crate::targets::NativeTarget;
use crate::vimarchy_policy::{Action, Policy};
use desktop_io::StableId;
use serde_json::Value;
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaximizeAction {
    Set,
    Unset,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaximizeRequest {
    pub workspace: i64,
    pub layout: String,
    pub fullscreen: u8,
    pub action: MaximizeAction,
    pub alt: bool,
    pub policy_digest: String,
}
impl MaximizeRequest {
    pub fn valid(&self) -> bool {
        self.workspace != 0
            && !self.layout.is_empty()
            && self.layout.len() <= 128
            && !self.layout.contains('\0')
            && self.fullscreen <= 3
            && self.policy_digest.len() == 64
            && self
                .policy_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && self.action
                == if self.fullscreen == 1 {
                    MaximizeAction::Unset
                } else {
                    MaximizeAction::Set
                }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaximizeProposal {
    target: NativeTarget,
    workspace: i64,
    layout: String,
    before_fullscreen: u8,
    action: MaximizeAction,
    source_revision: u64,
}
impl MaximizeProposal {
    pub fn request(&self, alt: bool, policy: &Policy) -> MaximizeRequest {
        MaximizeRequest {
            workspace: self.workspace,
            layout: self.layout.clone(),
            fullscreen: self.before_fullscreen,
            action: self.action,
            alt,
            policy_digest: policy.digest(),
        }
    }

    pub fn target(&self) -> &NativeTarget {
        &self.target
    }
    pub fn workspace(&self) -> i64 {
        self.workspace
    }
    pub fn layout(&self) -> &str {
        &self.layout
    }
    pub fn before_fullscreen(&self) -> u8 {
        self.before_fullscreen
    }
    pub fn action(&self) -> MaximizeAction {
        self.action
    }
    pub fn expected_fullscreen(&self) -> u8 {
        match self.action {
            MaximizeAction::Set => 1,
            MaximizeAction::Unset => 0,
        }
    }
    pub fn source_revision(&self) -> u64 {
        self.source_revision
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Disabled,
    CustomNotImplemented(Vec<String>),
    PromoteMasterNotImplemented,
    Maximize(MaximizeProposal),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSnapshot,
    TargetMissing,
}
/// Closed state extraction from pinned Hyprland clients/workspaces JSON. No
/// active-workspace assumption, raw address selector or arbitrary command text.
pub fn from_observations(
    clients: &Value,
    workspaces: &Value,
    target: &NativeTarget,
    source_revision: u64,
    alt: bool,
    policy: &Policy,
) -> Result<Decision, Error> {
    let bad = Error::InvalidSnapshot;
    if source_revision == 0 {
        return Err(bad);
    }
    let rows = clients
        .as_array()
        .filter(|rows| rows.len() <= 4096)
        .ok_or(bad)?;
    let mut ids = BTreeSet::new();
    let mut selected = None;
    for row in rows {
        let id = StableId::parse(row.get("stableId").and_then(Value::as_str).ok_or(bad)?)
            .map_err(|_| bad)?
            .canonical();
        if !ids.insert(id.clone()) {
            return Err(bad);
        }
        if id == target.stable_id() {
            selected = Some(row)
        }
    }
    let selected = selected.ok_or(Error::TargetMissing)?;
    if selected.get("mapped").and_then(Value::as_bool) != Some(true)
        || selected.get("hidden").and_then(Value::as_bool) != Some(false)
    {
        return Err(bad);
    }
    let workspace = selected
        .get("workspace")
        .and_then(|w| w.get("id"))
        .and_then(Value::as_i64)
        .filter(|id| *id != 0)
        .ok_or(bad)?;
    let before = selected
        .get("fullscreen")
        .and_then(Value::as_u64)
        .filter(|n| *n <= 3)
        .ok_or(bad)? as u8;
    let workspaces = workspaces
        .as_array()
        .filter(|rows| rows.len() <= 4096)
        .ok_or(bad)?;
    let mut seen = BTreeSet::new();
    let mut layout = None;
    for row in workspaces {
        let id = row.get("id").and_then(Value::as_i64).ok_or(bad)?;
        if !seen.insert(id) {
            return Err(bad);
        }
        if id == workspace {
            layout = Some(
                row.get("tiledLayout")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty() && s.len() <= 128 && !s.contains('\0'))
                    .ok_or(bad)?,
            )
        }
    }
    let layout = layout.ok_or(Error::TargetMissing)?;
    Ok(match policy.resolve(layout, alt) {
        Action::Disabled => Decision::Disabled,
        Action::Custom(argv) => Decision::CustomNotImplemented(argv),
        Action::PromoteMaster => Decision::PromoteMasterNotImplemented,
        Action::ToggleMaximized => Decision::Maximize(MaximizeProposal {
            target: target.clone(),
            workspace,
            layout: layout.into(),
            before_fullscreen: before,
            action: if before == 1 {
                MaximizeAction::Unset
            } else {
                MaximizeAction::Set
            },
            source_revision,
        }),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn target() -> NativeTarget {
        NativeTarget::new("fixture_1", "ab").unwrap()
    }
    fn clients(fullscreen: u8) -> Value {
        json!([{"stableId":"AB","mapped":true,"hidden":false,"workspace":{"id":2},"fullscreen":fullscreen}])
    }
    fn workspaces(layout: &str) -> Value {
        json!([{"id":1,"tiledLayout":"master"},{"id":2,"tiledLayout":layout}])
    }
    #[test]
    fn target_workspace_and_state_determine_explicit_action() {
        for before in 0..=3 {
            let Decision::Maximize(p) = from_observations(
                &clients(before),
                &workspaces("dwindle"),
                &target(),
                9,
                false,
                &Policy::default(),
            )
            .unwrap() else {
                panic!("maximize expected")
            };
            assert_eq!(p.workspace(), 2);
            assert_eq!(p.layout(), "dwindle");
            assert_eq!(p.before_fullscreen(), before);
            assert_eq!(p.expected_fullscreen(), if before == 1 { 0 } else { 1 });
            assert_eq!(p.source_revision(), 9);
        }
        assert_eq!(
            from_observations(
                &clients(0),
                &workspaces("master"),
                &target(),
                9,
                false,
                &Policy::default()
            )
            .unwrap(),
            Decision::PromoteMasterNotImplemented
        );
    }
    #[test]
    fn configured_disabled_custom_and_alt_precedence_cannot_be_bypassed() {
        let policy=Policy::parse(br#"{"doubleTap":{"layouts":{"*":"disabled"}},"altDoubleTap":{"layouts":{"dwindle":["sh","-c","echo explicit"],"*":"disabled"}}}"#).unwrap();
        assert_eq!(
            from_observations(
                &clients(0),
                &workspaces("dwindle"),
                &target(),
                1,
                false,
                &policy
            )
            .unwrap(),
            Decision::Disabled
        );
        assert_eq!(
            from_observations(
                &clients(0),
                &workspaces("dwindle"),
                &target(),
                1,
                true,
                &policy
            )
            .unwrap(),
            Decision::CustomNotImplemented(vec!["sh".into(), "-c".into(), "echo explicit".into()])
        );
        assert_eq!(
            from_observations(
                &clients(0),
                &workspaces("master"),
                &target(),
                1,
                true,
                &policy
            )
            .unwrap(),
            Decision::Disabled
        );
    }
    #[test]
    fn incomplete_duplicate_hidden_or_reused_observations_refuse() {
        let mut duplicate = clients(0);
        let row = duplicate[0].clone();
        duplicate.as_array_mut().unwrap().push(row);
        for rows in [duplicate, json!([]), json!([{"stableId":"ab"}]), clients(4)] {
            assert!(
                from_observations(
                    &rows,
                    &workspaces("dwindle"),
                    &target(),
                    1,
                    true,
                    &Policy::default()
                )
                .is_err()
            )
        }
        for rows in [
            json!([]),
            json!([{"id":2,"tiledLayout":"dwindle"},{"id":2,"tiledLayout":"master"}]),
            json!([{"id":2}]),
        ] {
            assert!(
                from_observations(&clients(0), &rows, &target(), 1, true, &Policy::default())
                    .is_err()
            )
        }
        let mut hidden = clients(0);
        hidden[0]["hidden"] = json!(true);
        assert!(
            from_observations(
                &hidden,
                &workspaces("dwindle"),
                &target(),
                1,
                true,
                &Policy::default()
            )
            .is_err()
        );
    }
}
