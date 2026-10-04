//! Read-only desktop observations, joined to independent arbiter-issued targets.
//! Observation identity cannot grant authority; submission uses modal-client.
use desktop_io::{Address, Endpoint, Events, Query, Snapshots, StableId};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use vimarchy_core::{
    gestures::Target,
    snapshot::{Client as WindowClient, Monitor, Snapshot},
};

#[derive(Debug)]
pub enum Error {
    Desktop(desktop_io::Error),
    Invalid,
    Stale,
    Exhausted,
    Arbiter(modal_client::Error),
}
pub struct Observation {
    monitors: Vec<Monitor>,
    clients: Vec<WindowClient>,
    snapshot: Snapshot,
    instance: String,
    revision: u64,
}
struct OwnFocus {
    fence: modal_contract::Fence,
    target: modal_runtime::targets::NativeTarget,
    address: String,
    legacy: Option<String>,
    seen_legacy: bool,
    seen_v2: bool,
    deadline: Instant,
}
impl OwnFocus {
    fn accept(&mut self, event: &desktop_io::Event) -> Result<(), Error> {
        match event.name.as_str() {
            "activewindow"
                if !self.seen_legacy && self.legacy.as_deref() == Some(event.payload.as_str()) =>
            {
                self.seen_legacy = true
            }
            "activewindowv2"
                if !self.seen_v2
                    && Address::parse(&event.payload)
                        .map(|a| a.canonical())
                        .ok()
                        .as_deref()
                        == Some(self.address.as_str()) =>
            {
                self.seen_v2 = true
            }
            _ => return Err(Error::Stale),
        }
        Ok(())
    }
}
fn focus_invariant(mut clients: serde_json::Value) -> Result<serde_json::Value, Error> {
    let rows = clients.as_array_mut().ok_or(Error::Invalid)?;
    for row in rows.iter_mut() {
        row.as_object_mut()
            .ok_or(Error::Invalid)?
            .remove("focusHistoryID");
    }
    // All fields except the explicitly mutable focus ranking remain compared.
    rows.sort_by_key(|r| {
        r.get("stableId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    });
    Ok(clients)
}
pub struct Native {
    endpoint: Endpoint,
    events: Events,
    snapshots: Snapshots,
    revision: u64,
    valid: bool,
    baseline_clients: serde_json::Value,
    baseline_monitors: serde_json::Value,
    own_focus: Option<OwnFocus>,
}
pub struct BoundView {
    view: modal_client::View,
    by_hint: BTreeMap<String, usize>,
    revision: u64,
}
impl Native {
    pub fn connect(endpoint: Endpoint, epoch: [u8; 16]) -> Result<Self, Error> {
        let events = endpoint
            .events(Duration::from_millis(500))
            .map_err(Error::Desktop)?;
        Ok(Self {
            endpoint,
            events,
            snapshots: Snapshots::new(epoch).map_err(Error::Desktop)?,
            revision: 0,
            valid: false,
            baseline_clients: serde_json::Value::Null,
            baseline_monitors: serde_json::Value::Null,
            own_focus: None,
        })
    }
    pub fn observe(&mut self) -> Result<Observation, Error> {
        self.valid = false;
        let result = self.observe_inner();
        if result.is_err() {
            self.snapshots.invalidate();
        }
        result
    }
    fn observe_inner(&mut self) -> Result<Observation, Error> {
        let deadline = Instant::now() + Duration::from_millis(500);
        let clients = self
            .snapshots
            .collect_clients(
                &self.endpoint,
                &mut self.events,
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(Error::Desktop)?;
        let monitors = self
            .endpoint
            .query(
                Query::Monitors,
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(Error::Desktop)?;
        if self
            .snapshots
            .poll(&mut self.events)
            .map_err(Error::Desktop)?
            .is_some()
            || self.events.has_partial()
        {
            return Err(Error::Stale);
        }
        self.baseline_clients = focus_invariant(clients.clone())?;
        self.baseline_monitors = monitors.clone();
        let mut clients: Vec<WindowClient> =
            serde_json::from_value(clients).map_err(|_| Error::Invalid)?;
        let monitors: Vec<Monitor> =
            serde_json::from_value(monitors).map_err(|_| Error::Invalid)?;
        let mut instance = None;
        for row in &mut clients {
            let address = Address::parse(&row.address).map_err(Error::Desktop)?;
            let identity = self.snapshots.identity(address).ok_or(Error::Stale)?;
            if instance.as_ref().is_some_and(|v| *v != identity.instance) {
                return Err(Error::Stale);
            }
            instance = Some(identity.instance);
            row.address = address.canonical();
            row.stable_id = StableId::parse(&row.stable_id)
                .map_err(Error::Desktop)?
                .canonical();
            for member in &mut row.grouped {
                *member = Address::parse(member).map_err(Error::Desktop)?.canonical();
            }
        }
        let snapshot =
            vimarchy_core::snapshot::normalize(&monitors, &clients).map_err(|_| Error::Invalid)?;
        self.revision = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        self.valid = true;
        Ok(Observation {
            monitors,
            clients,
            snapshot,
            instance: instance.unwrap_or_default(),
            revision: self.revision,
        })
    }
    /// Conservatively invalidate on any native event, including partial input.
    /// Call before accepting input and again immediately before effect submission.
    pub fn check_current(&mut self, revision: u64) -> Result<(), Error> {
        if !self.valid || revision != self.revision {
            return Err(Error::Stale);
        }
        match self.snapshots.poll(&mut self.events) {
            Ok(None) if !self.events.has_partial() => Ok(()),
            _ => {
                self.valid = false;
                self.snapshots.invalidate();
                Err(Error::Stale)
            }
        }
    }
    /// A finite reconciliation for exactly one acknowledged own focus. No event
    /// class is globally ignored, and the arbiter still revalidates every effect.
    pub fn reconcile_own_focus(
        &mut self,
        arbiter: &mut modal_client::Client,
        view: &mut BoundView,
    ) -> Result<(), Error> {
        let Some(mut own) = self.own_focus.take() else {
            return self.check_current(view.revision);
        };
        let result = (|| {
            if !self.valid
                || view.revision != self.revision
                || arbiter.lease().is_none_or(|lease| lease.fence != own.fence)
            {
                return Err(Error::Stale);
            }
            if Instant::now() >= own.deadline {
                return self.check_current(view.revision);
            }
            for _ in 0..3 {
                match self.events.poll_event().map_err(Error::Desktop)? {
                    Some(event) => own.accept(&event)?,
                    None => break,
                }
            }
            if self.events.has_partial() {
                return Err(Error::Stale);
            }
            let deadline = own
                .deadline
                .min(Instant::now() + Duration::from_millis(500));
            let query = |kind| {
                self.endpoint
                    .query(kind, deadline.saturating_duration_since(Instant::now()))
                    .map_err(Error::Desktop)
            };
            let active = query(Query::ActiveWindow)?;
            let id = active
                .get("stableId")
                .and_then(serde_json::Value::as_str)
                .ok_or(Error::Stale)?;
            let address = active
                .get("address")
                .and_then(serde_json::Value::as_str)
                .ok_or(Error::Stale)?;
            if StableId::parse(id).map_err(Error::Desktop)?.canonical() != own.target.stable_id()
                || Address::parse(address).map_err(Error::Desktop)?.canonical() != own.address
            {
                return Err(Error::Stale);
            }
            if focus_invariant(query(Query::Clients)?)? != self.baseline_clients
                || query(Query::Monitors)? != self.baseline_monitors
            {
                return Err(Error::Stale);
            }
            for _ in 0..3 {
                match self.events.poll_event().map_err(Error::Desktop)? {
                    Some(event) => own.accept(&event)?,
                    None => break,
                }
            }
            if self.events.has_partial() || Instant::now() >= deadline {
                return Err(Error::Stale);
            }
            let fresh = arbiter.targets().map_err(Error::Arbiter)?;
            let mut indexes = BTreeMap::new();
            for (hint, index) in &view.by_hint {
                let target = &view.view.rows().get(*index).ok_or(Error::Stale)?.target;
                let index = fresh
                    .rows()
                    .iter()
                    .position(|row| row.target == *target)
                    .ok_or(Error::Stale)?;
                indexes.insert(hint.clone(), index);
            }
            if Instant::now() >= deadline
                || fresh.rows().len() != view.view.rows().len()
                || view
                    .view
                    .rows()
                    .iter()
                    .any(|old| !fresh.rows().iter().any(|row| row.target == old.target))
            {
                return Err(Error::Stale);
            }
            view.view = fresh;
            view.by_hint = indexes;
            Ok(())
        })();
        if result.is_err() {
            self.valid = false;
            self.snapshots.invalidate();
        } else if !(own.seen_legacy && own.seen_v2) && Instant::now() < own.deadline {
            self.own_focus = Some(own);
        }
        result
    }
    pub fn bind(
        &mut self,
        observation: &Observation,
        view: modal_client::View,
    ) -> Result<BoundView, Error> {
        self.check_current(observation.revision)?;
        let rows = view
            .rows()
            .iter()
            .enumerate()
            .map(|(i, row)| ((row.target.instance(), row.target.stable_id()), i))
            .collect::<BTreeMap<_, _>>();
        let mut by_hint = BTreeMap::new();
        for window in &observation.snapshot.windows {
            let index = rows
                .get(&(observation.instance.as_str(), window.stable_id.as_str()))
                .ok_or(Error::Stale)?;
            if by_hint.insert(window.hint_id.clone(), *index).is_some() {
                return Err(Error::Invalid);
            }
        }
        Ok(BoundView {
            view,
            by_hint,
            revision: observation.revision,
        })
    }
    /// Fresh read-only action proposal. It is not accepted by the arbiter as
    /// authority and must never be converted directly into raw compositor IPC.
    pub fn plan_double_tap(
        &mut self,
        view: &BoundView,
        target: &Target,
        alt: bool,
        policy: &crate::policy::Policy,
    ) -> Result<crate::action_plan::Decision, Error> {
        self.check_current(view.revision)?;
        let index = *view.by_hint.get(&target.stable_id).ok_or(Error::Stale)?;
        let target = &view.view.rows().get(index).ok_or(Error::Stale)?.target;
        let deadline = Instant::now() + Duration::from_millis(500);
        let result = (|| {
            let clients = self
                .endpoint
                .query(
                    Query::Clients,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(Error::Desktop)?;
            let workspaces = self
                .endpoint
                .query(
                    Query::Workspaces,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(Error::Desktop)?;
            self.check_current(view.revision)?;
            if Instant::now() >= deadline {
                return Err(Error::Stale);
            }
            crate::action_plan::from_observations(
                &clients,
                &workspaces,
                target,
                view.revision,
                alt,
                policy,
            )
            .map_err(|_| Error::Invalid)
        })();
        if result.is_err() {
            self.valid = false;
            self.snapshots.invalidate();
        }
        result
    }
    /// Backend-stage proposal only; managed hold input is qualified separately.
    pub fn plan_workspace_move(
        &mut self,
        view: &BoundView,
        target: &Target,
        destination: u8,
        follow: bool,
    ) -> Result<modal_runtime::workspace_move::MoveWorkspaceRequest, Error> {
        self.check_current(view.revision)?;
        if !(1..=10).contains(&destination) {
            return Err(Error::Invalid);
        }
        let index = *view.by_hint.get(&target.stable_id).ok_or(Error::Stale)?;
        let target = &view.view.rows().get(index).ok_or(Error::Stale)?.target;
        let deadline = Instant::now() + Duration::from_millis(500);
        let result = (|| {
            let query = |kind| {
                self.endpoint
                    .query(kind, deadline.saturating_duration_since(Instant::now()))
                    .map_err(Error::Desktop)
            };
            let clients = query(Query::Clients)?;
            let workspaces = query(Query::Workspaces)?;
            let active = query(Query::ActiveWorkspace)?;
            self.check_current(view.revision)?;
            if Instant::now() >= deadline {
                return Err(Error::Stale);
            }
            modal_runtime::workspace_move::observe(
                &clients,
                &workspaces,
                &active,
                target,
                destination,
                follow,
            )
            .map_err(|_| Error::Invalid)
        })();
        if result.is_err() {
            self.valid = false;
            self.snapshots.invalidate();
        }
        result
    }
    pub fn move_workspace(
        &mut self,
        arbiter: &mut modal_client::Client,
        view: &BoundView,
        target: &Target,
        request: &modal_runtime::workspace_move::MoveWorkspaceRequest,
    ) -> Result<(), Error> {
        self.check_current(view.revision)?;
        let index = *view.by_hint.get(&target.stable_id).ok_or(Error::Stale)?;
        let result = arbiter
            .move_workspace(&view.view, index, request)
            .map_err(Error::Arbiter);
        // The displayed source cannot be reused after a possibly effectful move.
        self.valid = false;
        self.snapshots.invalidate();
        self.own_focus = None;
        result
    }
    pub fn maximize(
        &mut self,
        arbiter: &mut modal_client::Client,
        view: &BoundView,
        target: &Target,
        request: &crate::action_plan::MaximizeRequest,
    ) -> Result<(), Error> {
        self.check_current(view.revision)?;
        let index = *view.by_hint.get(&target.stable_id).ok_or(Error::Stale)?;
        arbiter
            .maximize(&view.view, index, request)
            .map_err(Error::Arbiter)
    }
    pub fn focus(
        &mut self,
        arbiter: &mut modal_client::Client,
        view: &BoundView,
        target: &Target,
    ) -> Result<(), Error> {
        self.check_current(view.revision)?;
        let index = *view.by_hint.get(&target.stable_id).ok_or(Error::Stale)?;
        let selected = &view.view.rows().get(index).ok_or(Error::Stale)?.target;
        let row = self
            .baseline_clients
            .as_array()
            .ok_or(Error::Stale)?
            .iter()
            .find(|r| {
                r.get("stableId")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| StableId::parse(id).ok())
                    .is_some_and(|id| id.canonical() == selected.stable_id())
            })
            .ok_or(Error::Stale)?;
        let address = Address::parse(
            row.get("address")
                .and_then(serde_json::Value::as_str)
                .ok_or(Error::Stale)?,
        )
        .map_err(Error::Desktop)?
        .canonical();
        let legacy = row
            .get("class")
            .and_then(serde_json::Value::as_str)
            .zip(row.get("title").and_then(serde_json::Value::as_str))
            .map(|(class, title)| format!("{class},{title}"));
        arbiter.focus(&view.view, index).map_err(Error::Arbiter)?;
        self.own_focus = Some(OwnFocus {
            fence: arbiter.lease().ok_or(Error::Stale)?.fence,
            target: selected.clone(),
            address,
            legacy,
            seen_legacy: false,
            seen_v2: false,
            deadline: Instant::now() + Duration::from_millis(600),
        });
        Ok(())
    }
}
impl Observation {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }
    pub fn clients(&self) -> &[WindowClient] {
        &self.clients
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

#[cfg(test)]
mod focus_reconciliation_tests {
    use super::*;
    fn expected() -> OwnFocus {
        OwnFocus {
            fence: modal_contract::Fence {
                authority_epoch: modal_contract::AuthorityEpoch([1; 16]),
                generation: std::num::NonZeroU64::new(1).unwrap(),
                owner: modal_contract::Owner {
                    client: modal_contract::ClientId([2; 16]),
                    client_epoch: std::num::NonZeroU64::new(1).unwrap(),
                },
            },
            target: modal_runtime::targets::NativeTarget::new("fixture", "ab").unwrap(),
            address: "0x1".into(),
            legacy: Some("app,title,comma".into()),
            seen_legacy: false,
            seen_v2: false,
            deadline: Instant::now() + Duration::from_millis(600),
        }
    }
    fn event(name: &str, payload: &str) -> desktop_io::Event {
        desktop_io::Event {
            name: name.into(),
            payload: payload.into(),
        }
    }
    #[test]
    fn exact_one_pair_in_either_order_and_no_deadline_extension() {
        for reversed in [false, true] {
            let mut own = expected();
            let deadline = own.deadline;
            let mut events = [
                event("activewindow", "app,title,comma"),
                event("activewindowv2", "1"),
            ];
            if reversed {
                events.reverse();
            }
            for e in events {
                own.accept(&e).unwrap();
            }
            assert!(own.seen_legacy && own.seen_v2);
            assert_eq!(own.deadline, deadline);
            assert!(own.accept(&event("activewindowv2", "1")).is_err());
        }
    }
    #[test]
    fn foreign_focus_state_events_and_unsupported_legacy_are_rejected() {
        for e in [
            event("activewindowv2", "2"),
            event("activewindow", "app,foreign"),
            event("closewindow", "1"),
            event("fullscreen", "1"),
            event("workspace", "2"),
        ] {
            assert!(expected().accept(&e).is_err());
        }
        let mut own = expected();
        own.legacy = None;
        assert!(
            own.accept(&event("activewindow", "app,title,comma"))
                .is_err()
        );
    }
    #[test]
    fn snapshot_comparison_ignores_only_focus_ranking() {
        let before = serde_json::json!([{"stableId":"ab","address":"0x1","fullscreen":0,"at":[1,2],"focusHistoryID":4}]);
        let mut changed = before.clone();
        changed[0]["focusHistoryID"] = 0.into();
        assert_eq!(
            focus_invariant(before.clone()).unwrap(),
            focus_invariant(changed.clone()).unwrap()
        );
        for field in ["stableId", "address", "fullscreen", "at"] {
            let mut invalid = changed.clone();
            invalid[0][field] = serde_json::Value::Null;
            assert_ne!(
                focus_invariant(before.clone()).unwrap(),
                focus_invariant(invalid).unwrap()
            );
        }
    }
}
