//! Closed workspace identity and request validation. No compositor authority.
use crate::targets::NativeTarget;
use desktop_io::{Address, StableId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidObservation;
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIdentity {
    pub id: i64,
    pub name: String,
    pub monitor_id: i64,
}
impl WorkspaceIdentity {
    pub fn valid(&self) -> bool {
        self.id != 0
            && self.monitor_id >= 0
            && !self.name.is_empty()
            && self.name.len() <= 256
            && !self.name.contains('\0')
    }
    fn numeric(&self) -> bool {
        self.valid() && (1..=10).contains(&self.id) && self.name == self.id.to_string()
    }
    fn parse(value: &Value) -> Result<Self, InvalidObservation> {
        let identity = Self {
            id: value
                .get("id")
                .and_then(Value::as_i64)
                .ok_or(InvalidObservation)?,
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or(InvalidObservation)?
                .into(),
            monitor_id: value
                .get("monitorID")
                .and_then(Value::as_i64)
                .ok_or(InvalidObservation)?,
        };
        if identity.valid() {
            Ok(identity)
        } else {
            Err(InvalidObservation)
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoveWorkspaceRequest {
    pub source: WorkspaceIdentity,
    pub destination: WorkspaceIdentity,
    pub active: WorkspaceIdentity,
    pub follow: bool,
}
impl MoveWorkspaceRequest {
    pub fn valid(&self) -> bool {
        self.source.numeric()
            && self.destination.numeric()
            && self.active.numeric()
            && self.source.monitor_id == self.destination.monitor_id
            && self.active.monitor_id == self.destination.monitor_id
    }
}
/// Restricted first stage: canonical numeric source, destination and active workspace (1–10);
/// no grouped, pinned,
/// hidden or fullscreen target and no implicit workspace creation.
pub fn observe(
    clients: &Value,
    workspaces: &Value,
    active: &Value,
    target: &NativeTarget,
    destination: u8,
    follow: bool,
) -> Result<MoveWorkspaceRequest, InvalidObservation> {
    if !(1..=10).contains(&destination) {
        return Err(InvalidObservation);
    }
    let mut seen = BTreeSet::new();
    let mut selected = None;
    for row in clients
        .as_array()
        .filter(|r| r.len() <= 4096)
        .ok_or(InvalidObservation)?
    {
        let id = StableId::parse(
            row.get("stableId")
                .and_then(Value::as_str)
                .ok_or(InvalidObservation)?,
        )
        .map_err(|_| InvalidObservation)?
        .canonical();
        if !seen.insert(id.clone()) {
            return Err(InvalidObservation);
        }
        if id == target.stable_id() {
            selected = Some(row)
        }
    }
    let row = selected.ok_or(InvalidObservation)?;
    if row.get("mapped").and_then(Value::as_bool) != Some(true)
        || row.get("hidden").and_then(Value::as_bool) != Some(false)
        || row.get("pinned").and_then(Value::as_bool) != Some(false)
        || row.get("fullscreen").and_then(Value::as_u64) != Some(0)
    {
        return Err(InvalidObservation);
    }
    if !matches!(
        row.get("swallowing").and_then(Value::as_str),
        Some("0" | "0x0")
    ) {
        return Err(InvalidObservation);
    }
    let address = Address::parse(
        row.get("address")
            .and_then(Value::as_str)
            .ok_or(InvalidObservation)?,
    )
    .map_err(|_| InvalidObservation)?;
    let grouped = row
        .get("grouped")
        .and_then(Value::as_array)
        .ok_or(InvalidObservation)?;
    if !grouped.is_empty()
        && !(grouped.len() == 1
            && grouped[0].as_str().and_then(|v| Address::parse(v).ok()) == Some(address))
    {
        return Err(InvalidObservation);
    }
    let current = row.get("workspace").ok_or(InvalidObservation)?;
    let source_id = current
        .get("id")
        .and_then(Value::as_i64)
        .ok_or(InvalidObservation)?;
    let source_name = current
        .get("name")
        .and_then(Value::as_str)
        .ok_or(InvalidObservation)?;
    let mut ids = BTreeSet::new();
    let mut source = None;
    let mut dest = None;
    for row in workspaces
        .as_array()
        .filter(|r| r.len() <= 4096)
        .ok_or(InvalidObservation)?
    {
        let workspace = WorkspaceIdentity::parse(row)?;
        if !ids.insert(workspace.id) {
            return Err(InvalidObservation);
        }
        if workspace.id == source_id {
            if workspace.name != source_name {
                return Err(InvalidObservation);
            }
            source = Some(workspace.clone());
        }
        if workspace.id == i64::from(destination) {
            dest = Some(workspace);
        }
    }
    let active = WorkspaceIdentity::parse(active)?;
    if !workspaces
        .as_array()
        .ok_or(InvalidObservation)?
        .iter()
        .any(|r| WorkspaceIdentity::parse(r).ok().as_ref() == Some(&active))
    {
        return Err(InvalidObservation);
    }
    let request = MoveWorkspaceRequest {
        source: source.ok_or(InvalidObservation)?,
        destination: dest.ok_or(InvalidObservation)?,
        active,
        follow,
    };
    if !request.valid()
        || row.get("monitor").and_then(Value::as_i64) != Some(request.source.monitor_id)
    {
        return Err(InvalidObservation);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn target() -> NativeTarget {
        NativeTarget::new("fixture", "ab").unwrap()
    }
    fn clients() -> Value {
        json!([{"stableId":"ab","address":"0x1","mapped":true,"hidden":false,"pinned":false,"swallowing":"0x0","grouped":[],"fullscreen":0,"monitor":0,"workspace":{"id":2,"name":"2"}}])
    }
    fn workspaces() -> Value {
        json!([{"id":2,"name":"2","monitorID":0},{"id":3,"name":"3","monitorID":0}])
    }
    fn active() -> Value {
        json!({"id":2,"name":"2","monitorID":0})
    }
    #[test]
    fn numeric_identity_and_monitor_scope_are_explicit() {
        let plan = observe(&clients(), &workspaces(), &active(), &target(), 3, false).unwrap();
        assert_eq!(plan.source.id, 2);
        assert_eq!(plan.destination.id, 3);
        assert!(!plan.follow);
        for destination in [0, 1, 11, 255] {
            assert!(
                observe(
                    &clients(),
                    &workspaces(),
                    &active(),
                    &target(),
                    destination,
                    true
                )
                .is_err()
            );
        }
        let mut moved = workspaces();
        moved[1]["monitorID"] = 1.into();
        assert!(observe(&clients(), &moved, &active(), &target(), 3, true).is_err());
    }
    #[test]
    fn duplicated_identity_missing_state_and_implicit_dependents_refuse() {
        let mut duplicate = clients();
        let row = duplicate[0].clone();
        duplicate.as_array_mut().unwrap().push(row);
        assert!(observe(&duplicate, &workspaces(), &active(), &target(), 3, false).is_err());
        let mut ws = workspaces();
        let row = ws[0].clone();
        ws.as_array_mut().unwrap().push(row);
        assert!(observe(&clients(), &ws, &active(), &target(), 3, false).is_err());
        for (field, bad) in [
            ("pinned", json!(true)),
            ("fullscreen", json!(1)),
            ("hidden", json!(true)),
            ("mapped", json!(false)),
            ("swallowing", json!("0x2")),
            ("grouped", json!(["0x1", "0x2"])),
            ("monitor", json!(1)),
            ("workspace", json!({"id":2,"name":"renamed"})),
        ] {
            let mut rows = clients();
            rows[0][field] = bad;
            assert!(
                observe(&rows, &workspaces(), &active(), &target(), 3, false).is_err(),
                "{field}"
            );
        }
        let mut missing = clients();
        missing[0].as_object_mut().unwrap().remove("pinned");
        assert!(observe(&missing, &workspaces(), &active(), &target(), 3, false).is_err());
    }
}

#[cfg(test)]
mod numeric_scope_regressions {
    use super::*;
    use serde_json::json;
    #[test]
    fn source_and_active_must_be_canonical_numeric_workspaces() {
        for (id, name) in [
            (-99, "special:stash"),
            (0, "0"),
            (11, "11"),
            (2, "named"),
            (2, "02"),
        ] {
            let mut clients = json!([{"stableId":"ab","address":"0x1","mapped":true,"hidden":false,"pinned":false,"swallowing":"0x0","grouped":[],"fullscreen":0,"monitor":0,"workspace":{"id":id,"name":name}}]);
            let ws = json!([{"id":id,"name":name,"monitorID":0},{"id":3,"name":"3","monitorID":0}]);
            let active = json!({"id":3,"name":"3","monitorID":0});
            let target = NativeTarget::new("fixture", "ab").unwrap();
            assert!(
                observe(&clients, &ws, &active, &target, 3, true).is_err(),
                "source {id}/{name}"
            );
            clients[0]["workspace"] = json!({"id":3,"name":"3"});
            assert!(
                observe(&clients, &ws, &ws[0], &target, 3, true).is_err(),
                "active {id}/{name}"
            );
        }
    }
}
