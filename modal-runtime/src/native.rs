//! Version-pinned Hyprland focus bracket. There is no atomic compare-and-dispatch
//! in native IPC: success is a matching fresh query and post-effect readback.
use crate::actor::{Application, Backend, Error};
use crate::host::Host;
use crate::targets::{MAX_TARGETS, NativeTarget, SourceTicket};
use desktop_io::{Endpoint, Events, ProcessIdentity, Query};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const SUPPORTED_COMMIT: &str = "efb50993780079460b0cbed1363e2166a2de1d9f";
fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(Error::Unavailable)
}
trait NativeIo {
    fn query(&mut self, query: Query, deadline: Instant) -> Result<Value, Error>;
    fn event(&mut self) -> Result<Option<String>, Error>;
    fn partial(&self) -> bool;
    fn submap(&mut self, target: desktop_io::ModalSubmap, deadline: Instant) -> Result<(), Error>;
    fn focus(&mut self, target: &NativeTarget, deadline: Instant) -> Result<(), Error>;
    fn move_event(&mut self) -> Result<Option<desktop_io::Event>, Error> {
        self.event().map(|e| {
            e.map(|name| desktop_io::Event {
                name,
                payload: String::new(),
            })
        })
    }
    fn supports_workspace_move(&self) -> bool {
        false
    }
    fn move_workspace(
        &mut self,
        _: &NativeTarget,
        _: u8,
        _: bool,
        _: Instant,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn supports_maximize(&self) -> bool {
        false
    }
    fn maximize(&mut self, _: &NativeTarget, _: bool, _: Instant) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
}
struct HyprIo {
    endpoint: Endpoint,
    events: Events,
    dialect: desktop_io::DispatchDialect,
}
impl NativeIo for HyprIo {
    fn move_event(&mut self) -> Result<Option<desktop_io::Event>, Error> {
        self.events.poll_event().map_err(|_| Error::Unavailable)
    }
    fn supports_workspace_move(&self) -> bool {
        matches!(self.dialect, desktop_io::DispatchDialect::Lua)
    }
    fn move_workspace(
        &mut self,
        target: &NativeTarget,
        destination: u8,
        follow: bool,
        deadline: Instant,
    ) -> Result<(), Error> {
        let id = desktop_io::StableId::parse(target.stable_id()).map_err(|_| Error::Unavailable)?;
        self.endpoint
            .move_to_workspace(id, destination, follow, self.dialect, deadline)
            .map_err(|_| Error::EffectUnconfirmed)
    }

    fn supports_maximize(&self) -> bool {
        matches!(self.dialect, desktop_io::DispatchDialect::Lua)
    }
    fn maximize(
        &mut self,
        target: &NativeTarget,
        set: bool,
        deadline: Instant,
    ) -> Result<(), Error> {
        let id = desktop_io::StableId::parse(target.stable_id()).map_err(|_| Error::Unavailable)?;
        self.endpoint
            .set_maximized(id, set, self.dialect, deadline)
            .map_err(|_| Error::EffectUnconfirmed)
    }

    fn query(&mut self, query: Query, deadline: Instant) -> Result<Value, Error> {
        self.endpoint
            .query(query, remaining(deadline)?)
            .map_err(|_| Error::Unavailable)
    }
    fn event(&mut self) -> Result<Option<String>, Error> {
        self.events
            .poll_event()
            .map(|event| event.map(|e| e.name))
            .map_err(|_| Error::Unavailable)
    }
    fn partial(&self) -> bool {
        self.events.has_partial()
    }
    fn submap(&mut self, target: desktop_io::ModalSubmap, deadline: Instant) -> Result<(), Error> {
        self.endpoint
            .set_modal_submap(target, self.dialect, deadline)
            .map_err(|_| Error::EffectUnconfirmed)
    }
    fn focus(&mut self, target: &NativeTarget, deadline: Instant) -> Result<(), Error> {
        let id = desktop_io::StableId::parse(target.stable_id()).map_err(|_| Error::Unavailable)?;
        self.endpoint
            .focus_stable_id(id, self.dialect, deadline)
            .map_err(|_| Error::EffectUnconfirmed)
    }
}
struct Guard<I> {
    io: I,
    instance: String,
    ready: bool,
    uncertain: bool,
    suspend: crate::suspend::SuspendGuard,
}
impl<I: NativeIo> Guard<I> {
    fn new(mut io: I, instance: String, deadline: Instant) -> Result<Self, Error> {
        let version = io.query(Query::Version, deadline)?;
        if version.get("commit").and_then(Value::as_str) != Some(SUPPORTED_COMMIT)
            || version.get("dirty").and_then(Value::as_bool) != Some(false)
        {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            io,
            instance,
            ready: false,
            uncertain: false,
            suspend: crate::suspend::SuspendGuard::new().map_err(|_| Error::Unavailable)?,
        })
    }
    fn transition(
        &mut self,
        owned: &mut Option<desktop_io::ModalSubmap>,
        target: desktop_io::ModalSubmap,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.suspend.check().map_err(|_| Error::Unavailable)?;
        if self.uncertain {
            return Err(Error::EffectUnconfirmed);
        }
        let current = self.io.query(Query::Submap, deadline)?;
        let current = current.as_str().ok_or(Error::Unavailable)?;
        let expected = owned.map(submap_name).unwrap_or("default");
        if current != "default" && current != expected {
            return Err(Error::Unavailable);
        }
        if current == submap_name(target) {
            remaining(deadline)?;
            if matches!(target, desktop_io::ModalSubmap::Reset) {
                *owned = None;
            }
            return Ok(());
        }
        if !matches!(target, desktop_io::ModalSubmap::Reset) {
            *owned = Some(target);
        }
        self.suspend.check().map_err(|_| Error::Unavailable)?;
        self.uncertain = true;
        self.io
            .submap(target, deadline)
            .map_err(|_| Error::EffectUnconfirmed)?;
        let after = self
            .io
            .query(Query::Submap, deadline)
            .map_err(|_| Error::EffectUnconfirmed)?;
        if after.as_str() != Some(submap_name(target)) || remaining(deadline).is_err() {
            return Err(Error::EffectUnconfirmed);
        }
        self.suspend.check().map_err(|_| Error::EffectUnconfirmed)?;
        self.uncertain = false;
        if matches!(target, desktop_io::ModalSubmap::Reset) {
            *owned = None;
        }
        Ok(())
    }
    fn rows(&mut self, deadline: Instant) -> Result<Vec<NativeTarget>, Error> {
        let value = self.io.query(Query::Clients, deadline)?;
        let rows = value
            .as_array()
            .filter(|rows| rows.len() <= MAX_TARGETS)
            .ok_or(Error::Unavailable)?;
        let targets = rows
            .iter()
            .map(|row| {
                row.get("stableId")
                    .and_then(Value::as_str)
                    .ok_or(Error::Unavailable)
                    .and_then(|id| NativeTarget::new(&self.instance, id).map_err(Error::Target))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if targets.iter().collect::<BTreeSet<_>>().len() != targets.len() {
            return Err(Error::Unavailable);
        }
        Ok(targets)
    }
    fn changed(&mut self) -> Result<bool, Error> {
        let mut changed = false;
        // Finite event drain. Never claim synchronized after exhausting the bound.
        for _ in 0..256 {
            match self.io.event()? {
                Some(_) => changed = true,
                None => {
                    return if self.io.partial() {
                        Err(Error::Unavailable)
                    } else {
                        Ok(changed)
                    };
                }
            }
        }
        Err(Error::Unavailable)
    }
    fn snapshot(&mut self, deadline: Instant) -> Result<Vec<NativeTarget>, Error> {
        self.ready = false;
        self.changed()?;
        let rows = self.rows(deadline)?;
        if self.changed()? {
            return Err(Error::Unavailable);
        }
        remaining(deadline)?;
        self.ready = true;
        Ok(rows)
    }
    fn move_workspace(
        &mut self,
        target: &NativeTarget,
        expected: &crate::workspace_move::MoveWorkspaceRequest,
        deadline: Instant,
    ) -> Result<(), Error> {
        use crate::workspace_move::observe;
        if self.uncertain
            || !self.ready
            || target.instance() != self.instance
            || !self.io.supports_workspace_move()
            || !expected.valid()
        {
            return Err(Error::Unavailable);
        }
        self.ready = false;
        if self.changed()? {
            return Err(Error::Unavailable);
        }
        let clients = self.io.query(Query::Clients, deadline)?;
        let workspaces = self.io.query(Query::Workspaces, deadline)?;
        let active = self.io.query(Query::ActiveWorkspace, deadline)?;
        let plan = observe(
            &clients,
            &workspaces,
            &active,
            target,
            expected.destination.id as u8,
            expected.follow,
        )
        .map_err(|_| Error::Unavailable)?;
        if plan != *expected || self.changed()? {
            return Err(Error::Unavailable);
        }
        remaining(deadline)?;
        self.suspend.check().map_err(|_| Error::Unavailable)?;
        if plan.source == plan.destination {
            self.ready = true;
            return Ok(());
        }
        let selected = clients
            .as_array()
            .ok_or(Error::Unavailable)?
            .iter()
            .find(|r| {
                r.get("stableId")
                    .and_then(Value::as_str)
                    .and_then(|s| desktop_io::StableId::parse(s).ok())
                    .is_some_and(|s| s.canonical() == target.stable_id())
            })
            .ok_or(Error::Unavailable)?;
        let address = desktop_io::Address::parse(
            selected
                .get("address")
                .and_then(Value::as_str)
                .ok_or(Error::Unavailable)?,
        )
        .map_err(|_| Error::Unavailable)?
        .canonical();
        self.uncertain = true;
        self.io
            .move_workspace(
                target,
                expected.destination.id as u8,
                expected.follow,
                deadline,
            )
            .map_err(|_| Error::EffectUnconfirmed)?;
        let result = (|| {
            let clients = self.io.query(Query::Clients, deadline)?;
            let workspaces = self.io.query(Query::Workspaces, deadline)?;
            let active = self.io.query(Query::ActiveWorkspace, deadline)?;
            let after = observe(
                &clients,
                &workspaces,
                &active,
                target,
                expected.destination.id as u8,
                expected.follow,
            )
            .map_err(|_| Error::Unavailable)?;
            let expected_active = if expected.follow {
                &expected.destination
            } else {
                &expected.active
            };
            if after.source != expected.destination
                || after.destination != expected.destination
                || &after.active != expected_active
            {
                return Err(Error::Unavailable);
            }
            if workspace_target_address(&clients, target)? != address {
                return Err(Error::Unavailable);
            }
            let active_window = self.io.query(Query::ActiveWindow, deadline)?;
            if expected.follow && !workspace_focus_matches(&active_window, target, &address) {
                return Err(Error::Unavailable);
            }
            let mut seen = BTreeSet::new();
            for _ in 0..32 {
                match self.io.move_event()? {
                    Some(event) => {
                        if !seen.insert(event.name.clone())
                            || !workspace_completion_event(
                                &event,
                                &address,
                                expected,
                                &active_window,
                            )
                        {
                            return Err(Error::Unavailable);
                        }
                    }
                    None => {
                        if self.io.partial() {
                            return Err(Error::Unavailable);
                        }
                        // Requery after draining notifications. Notifications never certify completion.
                        let clients = self.io.query(Query::Clients, deadline)?;
                        let workspaces = self.io.query(Query::Workspaces, deadline)?;
                        let active = self.io.query(Query::ActiveWorkspace, deadline)?;
                        let last = observe(
                            &clients,
                            &workspaces,
                            &active,
                            target,
                            expected.destination.id as u8,
                            expected.follow,
                        )
                        .map_err(|_| Error::Unavailable)?;
                        if expected.follow {
                            let final_focus = self.io.query(Query::ActiveWindow, deadline)?;
                            if !workspace_focus_matches(&final_focus, target, &address) {
                                return Err(Error::Unavailable);
                            }
                        }
                        if last != after
                            || workspace_target_address(&clients, target)? != address
                            || self.changed()?
                        {
                            return Err(Error::Unavailable);
                        }
                        remaining(deadline)?;
                        return Ok(());
                    }
                }
            }
            Err(Error::Unavailable)
        })();
        result.map_err(|_| Error::EffectUnconfirmed)?;
        self.suspend.check().map_err(|_| Error::EffectUnconfirmed)?;
        self.uncertain = false;
        // Moving changed this observation; only a fresh observer snapshot can re-enable effects.
        Ok(())
    }
    fn maximize(
        &mut self,
        target: &NativeTarget,
        expected: &crate::vimarchy_action::MaximizeRequest,
        policy: &crate::vimarchy_policy::Policy,
        deadline: Instant,
    ) -> Result<(), Error> {
        use crate::vimarchy_action::{Decision, MaximizeAction, from_observations};
        if self.uncertain
            || !self.ready
            || target.instance() != self.instance
            || !self.io.supports_maximize()
            || !expected.valid()
            || expected.policy_digest != policy.digest()
        {
            return Err(Error::Unavailable);
        }
        self.ready = false;
        if self.changed()? {
            return Err(Error::Unavailable);
        }
        let clients = self.io.query(Query::Clients, deadline)?;
        let workspaces = self.io.query(Query::Workspaces, deadline)?;
        let Decision::Maximize(plan) =
            from_observations(&clients, &workspaces, target, 1, expected.alt, policy)
                .map_err(|_| Error::Unavailable)?
        else {
            return Err(Error::Unavailable);
        };
        if plan.request(expected.alt, policy) != *expected || self.changed()? {
            return Err(Error::Unavailable);
        }
        remaining(deadline)?;
        self.suspend.check().map_err(|_| Error::Unavailable)?;
        self.uncertain = true;
        self.io
            .maximize(
                target,
                matches!(expected.action, MaximizeAction::Set),
                deadline,
            )
            .map_err(|_| Error::EffectUnconfirmed)?;
        let result = (|| {
            let clients = self.io.query(Query::Clients, deadline)?;
            let workspaces = self.io.query(Query::Workspaces, deadline)?;
            let Decision::Maximize(after) =
                from_observations(&clients, &workspaces, target, 1, expected.alt, policy)
                    .map_err(|_| Error::Unavailable)?
            else {
                return Err(Error::Unavailable);
            };
            if after.workspace() != plan.workspace()
                || after.layout() != plan.layout()
                || after.before_fullscreen() != plan.expected_fullscreen()
            {
                return Err(Error::Unavailable);
            }
            for _ in 0..256 {
                match self.io.event()? {
                    Some(name) if name == "fullscreen" => {}
                    Some(_) => return Err(Error::Unavailable),
                    None => {
                        if self.io.partial() {
                            return Err(Error::Unavailable);
                        }
                        remaining(deadline)?;
                        return Ok(());
                    }
                }
            }
            Err(Error::Unavailable)
        })();
        result.map_err(|_| Error::EffectUnconfirmed)?;
        self.suspend.check().map_err(|_| Error::EffectUnconfirmed)?;
        self.uncertain = false;
        self.ready = true;
        Ok(())
    }
    fn focus(&mut self, target: &NativeTarget, deadline: Instant) -> Result<(), Error> {
        if self.uncertain || !self.ready || target.instance() != self.instance {
            return Err(Error::Unavailable);
        }
        // Any failed native bracket poisons this observation view.
        self.ready = false;
        if self.changed()? {
            return Err(Error::Unavailable);
        }
        if !self.rows(deadline)?.contains(target) {
            return Err(Error::Unavailable);
        }
        if self.changed()? {
            return Err(Error::Unavailable);
        }
        remaining(deadline)?;
        // Once submitted, every error is explicitly possibly-effectful.
        self.suspend.check().map_err(|_| Error::Unavailable)?;
        self.uncertain = true;
        self.io
            .focus(target, deadline)
            .map_err(|_| Error::EffectUnconfirmed)?;
        let result = (|| {
            let active = self.io.query(Query::ActiveWindow, deadline)?;
            let stable = active
                .get("stableId")
                .and_then(Value::as_str)
                .ok_or(Error::Unavailable)?;
            if NativeTarget::new(&self.instance, stable).map_err(Error::Target)? != *target {
                return Err(Error::Unavailable);
            }
            if !self.rows(deadline)?.contains(target) {
                return Err(Error::Unavailable);
            }
            // Only focus notifications are harmless inside the completion bracket.
            for _ in 0..256 {
                match self.io.event()? {
                    Some(name) if matches!(name.as_str(), "activewindow" | "activewindowv2") => {}
                    Some(_) => return Err(Error::Unavailable),
                    None => {
                        if self.io.partial() {
                            return Err(Error::Unavailable);
                        }
                        remaining(deadline)?;
                        return Ok(());
                    }
                }
            }
            Err(Error::Unavailable)
        })();
        result.map_err(|_| Error::EffectUnconfirmed)?;
        self.suspend.check().map_err(|_| Error::EffectUnconfirmed)?;
        self.uncertain = false;
        self.ready = true;
        Ok(())
    }
}
fn workspace_focus_matches(active: &Value, target: &NativeTarget, address: &str) -> bool {
    active
        .get("stableId")
        .and_then(Value::as_str)
        .and_then(|s| desktop_io::StableId::parse(s).ok())
        .is_some_and(|s| s.canonical() == target.stable_id())
        && active
            .get("address")
            .and_then(Value::as_str)
            .and_then(|s| desktop_io::Address::parse(s).ok())
            == desktop_io::Address::parse(address).ok()
}

fn workspace_target_address(clients: &Value, target: &NativeTarget) -> Result<String, Error> {
    let row = clients
        .as_array()
        .ok_or(Error::Unavailable)?
        .iter()
        .find(|r| {
            r.get("stableId")
                .and_then(Value::as_str)
                .and_then(|s| desktop_io::StableId::parse(s).ok())
                .is_some_and(|s| s.canonical() == target.stable_id())
        })
        .ok_or(Error::Unavailable)?;
    desktop_io::Address::parse(
        row.get("address")
            .and_then(Value::as_str)
            .ok_or(Error::Unavailable)?,
    )
    .map(|a| a.canonical())
    .map_err(|_| Error::Unavailable)
}
fn workspace_completion_event(
    event: &desktop_io::Event,
    address: &str,
    expected: &crate::workspace_move::MoveWorkspaceRequest,
    active: &Value,
) -> bool {
    let bare = address.strip_prefix("0x").unwrap_or(address);
    match event.name.as_str() {
        "movewindow" => event.payload == format!("{bare},{}", expected.destination.name),
        "movewindowv2" => {
            event.payload
                == format!(
                    "{bare},{},{}",
                    expected.destination.id, expected.destination.name
                )
        }
        "workspace" => expected.follow && event.payload == expected.destination.name,
        "workspacev2" => {
            expected.follow
                && event.payload
                    == format!("{},{}", expected.destination.id, expected.destination.name)
        }
        "activewindow" => active
            .get("class")
            .and_then(Value::as_str)
            .zip(active.get("title").and_then(Value::as_str))
            .is_some_and(|(class, title)| event.payload == format!("{class},{title}")),
        "activewindowv2" => active
            .get("address")
            .and_then(Value::as_str)
            .and_then(|s| desktop_io::Address::parse(s).ok())
            .zip(desktop_io::Address::parse(&event.payload).ok())
            .is_some_and(|(a, b)| a == b),
        _ => false,
    }
}
/// Shared only within the single-threaded modal actor host. No client can publish
/// observations. Connection death requires constructing a fresh source and ticket.
#[derive(Clone)]
pub struct NativeFocus(Rc<RefCell<Guard<HyprIo>>>);
impl NativeFocus {
    pub fn connect(
        runtime: &Path,
        instance: &str,
        identity: ProcessIdentity,
        dialect: desktop_io::DispatchDialect,
        deadline: Instant,
    ) -> Result<Self, Error> {
        let endpoint = Endpoint::discover_identity(runtime, instance, identity)
            .map_err(|_| Error::Unavailable)?;
        let events = endpoint
            .events(remaining(deadline)?)
            .map_err(|_| Error::Unavailable)?;
        Ok(Self(Rc::new(RefCell::new(Guard::new(
            HyprIo {
                endpoint,
                events,
                dialect,
            },
            instance.into(),
            deadline,
        )?))))
    }
    /// Called synchronously by Compositor::focus while the actor holds the fence.
    /// This API alone grants no lease; NativeCompositor below is the integration.
    fn focus(&self, target: &NativeTarget, deadline: Instant) -> Result<(), Error> {
        self.0.borrow_mut().focus(target, deadline)
    }
    pub fn observer(&self) -> Observer {
        Observer {
            source: self.clone(),
            ticket: None,
            sequence: 0,
        }
    }
}
/// Host driver must call step instead of Host::step: event invalidation precedes
/// servicing client requests and focus performs another native bracket.
pub struct Observer {
    source: NativeFocus,
    ticket: Option<SourceTicket>,
    sequence: u64,
}
impl Observer {
    fn next(&mut self) -> Result<u64, Error> {
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Unavailable)?;
        Ok(self.sequence)
    }
    pub fn step<B: Backend, P: FnMut(u32, i32) -> Option<Application>>(
        &mut self,
        host: &mut Host<B, P>,
        deadline: Instant,
    ) -> Result<(), Error> {
        host.maintenance()?;
        let deadline = deadline.min(Instant::now() + Duration::from_millis(50));
        if self.ticket.is_none() {
            self.ticket = Some(host.begin_source()?);
            self.sequence = 0;
        }
        let ticket = self.ticket.ok_or(Error::Unavailable)?;
        let dirty = {
            let mut guard = self.source.0.borrow_mut();
            let ready = guard.ready;
            match guard.changed() {
                Ok(changed) => changed || !ready,
                Err(error) => {
                    guard.ready = false;
                    drop(guard);
                    host.lost(ticket, self.next()?)?;
                    return Err(error);
                }
            }
        };
        if dirty {
            host.lost(ticket, self.next()?)?;
            let rows = self.source.0.borrow_mut().snapshot(deadline)?;
            host.snapshot(ticket, self.next()?, rows)?;
        }
        host.step().map_err(|_| Error::Unavailable)
    }
}
/// Lifecycle readiness/dismissal remains supplied by a native surface adapter.
/// A missing native surface adapter must refuse readiness, never return a stub success.
pub struct NativeCompositor<C> {
    pub lifecycle: C,
    pub focus: NativeFocus,
}
impl<C: crate::backend::Compositor> crate::backend::Compositor for NativeCompositor<C> {
    fn move_workspace(
        &mut self,
        _: modal_contract::Fence,
        target: &NativeTarget,
        expected: &crate::workspace_move::MoveWorkspaceRequest,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.focus
            .0
            .borrow_mut()
            .move_workspace(target, expected, deadline)
    }

    fn maximize(
        &mut self,
        _: modal_contract::Fence,
        target: &NativeTarget,
        expected: &crate::vimarchy_action::MaximizeRequest,
        policy: &crate::vimarchy_policy::Policy,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.focus
            .0
            .borrow_mut()
            .maximize(target, expected, policy, deadline)
    }

    fn namespace(&self, fence: modal_contract::Fence) -> Option<String> {
        self.lifecycle.namespace(fence)
    }
    fn prepare(
        &mut self,
        fence: modal_contract::Fence,
    ) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, Error> {
        self.lifecycle.prepare(fence)
    }
    fn ready(
        &mut self,
        pid: u32,
        fence: modal_contract::Fence,
        deadline: Instant,
    ) -> Result<bool, Error> {
        self.lifecycle.ready(pid, fence, deadline)
    }
    fn dismissed(
        &mut self,
        fence: modal_contract::Fence,
        deadline: Instant,
    ) -> Result<bool, Error> {
        if self.focus.0.borrow().uncertain {
            return Err(Error::EffectUnconfirmed);
        }
        self.lifecycle.dismissed(fence, deadline)
    }
    fn dispatch(
        &mut self,
        fence: modal_contract::Fence,
        effect: crate::actor::Effect,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.lifecycle.dispatch(fence, effect, deadline)
    }
    fn focus(
        &mut self,
        _fence: modal_contract::Fence,
        target: &NativeTarget,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.focus.focus(target, deadline)
    }
}

/// Pinned HyprCtl.cpp emits all four layer arrays for every monitor. A partial
/// object is not a complete observation and cannot establish absence/readiness.
fn layer_levels(monitor: &Value) -> Result<&serde_json::Map<String, Value>, Error> {
    monitor
        .get("levels")
        .and_then(Value::as_object)
        .filter(|levels| {
            levels.len() == 4
                && ["0", "1", "2", "3"]
                    .iter()
                    .all(|key| levels.get(*key).is_some_and(Value::is_array))
        })
        .ok_or(Error::Unavailable)
}

/// Independent layer-surface observation, not a GTK callback or namespace-only check.
fn surface_present(value: &Value, pid: u32, namespace: &str) -> Result<bool, Error> {
    let monitors = value
        .as_object()
        .filter(|v| v.len() <= 64)
        .ok_or(Error::Unavailable)?;
    let mut matched = 0;
    let mut visible = false;
    let mut total = 0;
    for monitor in monitors.values() {
        let levels = layer_levels(monitor)?;
        for rows in levels.values() {
            for row in rows.as_array().ok_or(Error::Unavailable)? {
                total += 1;
                if total > 4096 {
                    return Err(Error::Unavailable);
                }
                let name = row
                    .get("namespace")
                    .and_then(Value::as_str)
                    .ok_or(Error::Unavailable)?;
                if name == namespace {
                    if row.get("pid").and_then(Value::as_u64) != Some(pid as u64)
                        || row.get("w").and_then(Value::as_f64).is_none_or(|n| n <= 0.)
                        || row.get("h").and_then(Value::as_f64).is_none_or(|n| n <= 0.)
                        || row
                            .get("alpha")
                            .and_then(Value::as_f64)
                            .is_none_or(|n| !n.is_finite() || !(0. ..=1.).contains(&n))
                    {
                        return Err(Error::Unavailable);
                    }
                    matched += 1;
                    // A freshly mapped native layer can still be at fade-in alpha zero.
                    // Keep waiting within the caller deadline; never grant readiness yet.
                    visible = row["alpha"].as_f64().is_some_and(|alpha| alpha > 0.);
                }
            }
        }
    }
    if matched > 1 {
        return Err(Error::Unavailable);
    }
    Ok(matched == 1 && visible)
}
fn namespace_absent(value: &Value, namespace: &str) -> Result<bool, Error> {
    let monitors = value
        .as_object()
        .filter(|v| v.len() <= 64)
        .ok_or(Error::Unavailable)?;
    let mut total = 0;
    for monitor in monitors.values() {
        for rows in layer_levels(monitor)?.values() {
            for row in rows.as_array().ok_or(Error::Unavailable)? {
                total += 1;
                if total > 4096 {
                    return Err(Error::Unavailable);
                }
                if row
                    .get("namespace")
                    .and_then(Value::as_str)
                    .ok_or(Error::Unavailable)?
                    == namespace
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}
/// PID-bound child report plus independent layers observation. This implements
/// surface lifecycle and source-derived closed submap transitions.
pub struct SurfaceLifecycle {
    root: std::path::PathBuf,
    source: NativeFocus,
    active: Option<(modal_contract::Fence, crate::readiness::Channel)>,
    submap: Option<desktop_io::ModalSubmap>,
}
impl SurfaceLifecycle {
    pub fn new(root: std::path::PathBuf, source: NativeFocus) -> Self {
        Self {
            root,
            source,
            active: None,
            submap: None,
        }
    }
}
fn submap_name(submap: desktop_io::ModalSubmap) -> &'static str {
    match submap {
        desktop_io::ModalSubmap::Vimarchy => "vimarchy",
        desktop_io::ModalSubmap::VimarchyDouble => "vimarchy-double",
        desktop_io::ModalSubmap::Ask => "omarchy-ask",
        desktop_io::ModalSubmap::Reset => "default",
    }
}
impl SurfaceLifecycle {
    fn current_submap(&self, deadline: Instant) -> Result<String, Error> {
        let value = self
            .source
            .0
            .borrow_mut()
            .io
            .query(Query::Submap, deadline)?;
        let name = value
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or(Error::Unavailable)?;
        Ok(name.into())
    }
    fn transition(
        &mut self,
        target: desktop_io::ModalSubmap,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.source
            .0
            .borrow_mut()
            .transition(&mut self.submap, target, deadline)
    }
}
impl crate::backend::Compositor for SurfaceLifecycle {
    fn namespace(&self, fence: modal_contract::Fence) -> Option<String> {
        self.active
            .as_ref()
            .filter(|(active, _)| *active == fence)
            .map(|(_, channel)| channel.namespace().to_owned())
    }
    fn prepare(
        &mut self,
        fence: modal_contract::Fence,
    ) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, Error> {
        if self.active.is_some() {
            return Err(Error::Unavailable);
        }
        let channel =
            crate::readiness::Channel::bind(&self.root, fence).map_err(|_| Error::Unavailable)?;
        let environment = channel.environment();
        self.active = Some((fence, channel));
        Ok(environment)
    }
    fn ready(
        &mut self,
        pid: u32,
        fence: modal_contract::Fence,
        deadline: Instant,
    ) -> Result<bool, Error> {
        let (active, channel) = self.active.as_mut().ok_or(Error::Unavailable)?;
        if *active != fence {
            return Err(Error::Unavailable);
        }
        channel
            .receive(pid, deadline)
            .map_err(|_| Error::Unavailable)?;
        loop {
            let layers = self
                .source
                .0
                .borrow_mut()
                .io
                .query(Query::Layers, deadline)?;
            if surface_present(&layers, pid, channel.namespace())? {
                remaining(deadline)?;
                if self.source.0.borrow().uncertain {
                    return Err(Error::Unavailable);
                }
                let current = self
                    .source
                    .0
                    .borrow_mut()
                    .io
                    .query(Query::Submap, deadline)?;
                if current.as_str() != Some("default") {
                    return Err(Error::Unavailable);
                }
                self.source
                    .0
                    .borrow_mut()
                    .suspend
                    .check()
                    .map_err(|_| Error::Unavailable)?;
                channel.activate(deadline).map_err(|_| Error::Unavailable)?;
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(2).min(remaining(deadline)?));
        }
    }
    fn dismissed(
        &mut self,
        fence: modal_contract::Fence,
        deadline: Instant,
    ) -> Result<bool, Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
        {
            return Err(Error::Unavailable);
        }
        if self.source.0.borrow().uncertain {
            return Err(Error::EffectUnconfirmed);
        }
        self.transition(desktop_io::ModalSubmap::Reset, deadline)?;
        let Some((active, channel)) = self.active.as_ref() else {
            return Err(Error::Unavailable);
        };
        if *active != fence {
            return Err(Error::Unavailable);
        }
        loop {
            let layers = self
                .source
                .0
                .borrow_mut()
                .io
                .query(Query::Layers, deadline)?;
            if namespace_absent(&layers, channel.namespace())? {
                remaining(deadline)?;
                self.active = None;
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(2).min(remaining(deadline)?));
        }
    }
    fn dispatch(
        &mut self,
        fence: modal_contract::Fence,
        effect: crate::actor::Effect,
        deadline: Instant,
    ) -> Result<(), Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
        {
            return Err(Error::Unavailable);
        }
        let target = match effect {
            crate::actor::Effect::EnterVimarchy => desktop_io::ModalSubmap::Vimarchy,
            crate::actor::Effect::EnterVimarchyDouble => desktop_io::ModalSubmap::VimarchyDouble,
            crate::actor::Effect::EnterAsk => desktop_io::ModalSubmap::Ask,
            crate::actor::Effect::Reset => desktop_io::ModalSubmap::Reset,
            crate::actor::Effect::EnterYoohoo => {
                // Yoohoo uses its already-activated presenter, not an invented submap.
                if self.submap.is_some()
                    || self.source.0.borrow().uncertain
                    || self.current_submap(deadline)? != "default"
                {
                    return Err(Error::Unavailable);
                }
                remaining(deadline)?;
                return Ok(());
            }
        };
        self.transition(target, deadline)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    struct Fake {
        queries: VecDeque<Value>,
        events: VecDeque<Result<Option<String>, Error>>,
        submitted: usize,
        partial: bool,
        fail_submit: bool,
        submaps: Vec<desktop_io::ModalSubmap>,
    }
    impl NativeIo for Fake {
        fn supports_maximize(&self) -> bool {
            true
        }
        fn maximize(
            &mut self,
            target: &NativeTarget,
            _: bool,
            deadline: Instant,
        ) -> Result<(), Error> {
            self.focus(target, deadline)
        }

        fn query(&mut self, _: Query, deadline: Instant) -> Result<Value, Error> {
            remaining(deadline)?;
            self.queries.pop_front().ok_or(Error::Unavailable)
        }
        fn event(&mut self) -> Result<Option<String>, Error> {
            self.events.pop_front().unwrap_or(Ok(None))
        }
        fn submap(
            &mut self,
            target: desktop_io::ModalSubmap,
            deadline: Instant,
        ) -> Result<(), Error> {
            remaining(deadline)?;
            self.submaps.push(target);
            if self.fail_submit {
                Err(Error::Unavailable)
            } else {
                Ok(())
            }
        }
        fn partial(&self) -> bool {
            self.partial
        }
        fn focus(&mut self, _: &NativeTarget, deadline: Instant) -> Result<(), Error> {
            remaining(deadline)?;
            self.submitted += 1;
            if self.fail_submit {
                Err(Error::Unavailable)
            } else {
                Ok(())
            }
        }
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
    fn target() -> NativeTarget {
        NativeTarget::new("instance", "18000000").unwrap()
    }
    fn rows() -> Value {
        json!([{"address":"0x1234","stableId":"18000000"}])
    }
    fn guard() -> Guard<Fake> {
        let mut g = Guard::new(
            Fake {
                queries: VecDeque::from([json!({"commit":SUPPORTED_COMMIT,"dirty":false}), rows()]),
                events: VecDeque::new(),
                submitted: 0,
                partial: false,
                fail_submit: false,
                submaps: Vec::new(),
            },
            "instance".into(),
            deadline(),
        )
        .unwrap();
        assert_eq!(g.snapshot(deadline()).unwrap(), vec![target()]);
        g
    }

    fn maximize_rows(state: u8) -> Value {
        json!([{"stableId":"18000000","mapped":true,"hidden":false,"workspace":{"id":2},"fullscreen":state}])
    }
    fn workspaces() -> Value {
        json!([{"id":2,"tiledLayout":"dwindle"}])
    }
    fn maximize_request(
        before: u8,
        policy: &crate::vimarchy_policy::Policy,
    ) -> crate::vimarchy_action::MaximizeRequest {
        let crate::vimarchy_action::Decision::Maximize(p) =
            crate::vimarchy_action::from_observations(
                &maximize_rows(before),
                &workspaces(),
                &target(),
                1,
                false,
                policy,
            )
            .unwrap()
        else {
            panic!()
        };
        p.request(false, policy)
    }
    #[test]
    fn maximize_explicit_set_unset_and_readback() {
        let policy = crate::vimarchy_policy::Policy::default();
        for before in 0..=3 {
            let mut g = guard();
            let after = if before == 1 { 0 } else { 1 };
            g.io.queries.extend([
                maximize_rows(before),
                workspaces(),
                maximize_rows(after),
                workspaces(),
            ]);
            g.maximize(
                &target(),
                &maximize_request(before, &policy),
                &policy,
                deadline(),
            )
            .unwrap();
            assert_eq!(g.io.submitted, 1);
            assert!(g.ready);
            assert!(!g.uncertain);
        }
    }
    #[test]
    fn maximize_changed_state_disabled_custom_and_policy_mismatch_never_submit() {
        let default = crate::vimarchy_policy::Policy::default();
        for raw in [
            br#"{}"#.as_slice(),
            br#"{"doubleTap":{"layouts":{"dwindle":"disabled"}}}"#,
            br#"{"doubleTap":{"layouts":{"dwindle":["custom"]}}}"#,
        ] {
            let policy = crate::vimarchy_policy::Policy::parse(raw).unwrap();
            let mut expected = maximize_request(0, &default);
            expected.policy_digest = policy.digest();
            let mut g = guard();
            g.io.queries.extend([maximize_rows(1), workspaces()]);
            assert!(
                g.maximize(&target(), &expected, &policy, deadline())
                    .is_err()
            );
            assert_eq!(g.io.submitted, 0);
        }
        let mut g = guard();
        let mut request = maximize_request(0, &default);
        request.policy_digest = "0".repeat(64);
        assert!(
            g.maximize(&target(), &request, &default, deadline())
                .is_err()
        );
        assert_eq!(g.io.submitted, 0);
    }
    #[test]
    fn maximize_lost_ack_or_bad_readback_latches_uncertainty() {
        let policy = crate::vimarchy_policy::Policy::default();
        for lost_ack in [true, false] {
            let mut g = guard();
            g.io.fail_submit = lost_ack;
            g.io.queries.extend([
                maximize_rows(0),
                workspaces(),
                maximize_rows(0),
                workspaces(),
            ]);
            let request = maximize_request(0, &policy);
            assert_eq!(
                g.maximize(&target(), &request, &policy, deadline()),
                Err(Error::EffectUnconfirmed)
            );
            assert!(g.uncertain);
            assert!(!g.ready);
            assert!(
                g.maximize(&target(), &request, &policy, deadline())
                    .is_err()
            );
            assert_eq!(g.io.submitted, 1);
        }
    }
    #[test]
    fn lost_clock_continuity_refuses_focus_and_submap_submission() {
        let mut g = guard();
        g.io.queries.push_back(rows());
        g.suspend.invalidate_for_test();
        assert!(g.focus(&target(), deadline()).is_err());
        assert_eq!(g.io.submitted, 0);
        assert!(
            g.transition(&mut None, desktop_io::ModalSubmap::Ask, deadline())
                .is_err()
        );
        assert!(g.io.submaps.is_empty());
    }
    #[test]
    fn fresh_stable_identity_and_bracket_readback_qualify_effect() {
        let mut g = guard();
        g.io.queries
            .extend([rows(), json!({"stableId":"18000000"}), rows()]);
        g.focus(&target(), deadline()).unwrap();
        assert_eq!(g.io.submitted, 1);
        assert!(g.ready);
    }
    #[test]
    fn raw_address_reuse_missing_stable_identity_never_submits() {
        for clients in [
            json!([{"address":"0x1234","stableId":"18000001"}]),
            json!([{"address":"0x1234"}]),
            json!([{"stableId":"18000000"},{"stableId":"18000000"}]),
        ] {
            let mut g = guard();
            g.io.queries.push_back(clients);
            assert!(g.focus(&target(), deadline()).is_err());
            assert_eq!(g.io.submitted, 0);
            assert!(!g.ready);
        }
    }
    #[test]
    fn lifecycle_event_partial_frame_and_lost_stream_refuse_before_submit() {
        for event in [Ok(Some("closewindow".into())), Err(Error::Unavailable)] {
            let mut g = guard();
            g.io.events.push_back(event);
            assert!(g.focus(&target(), deadline()).is_err());
            assert_eq!(g.io.submitted, 0);
        }
        let mut g = guard();
        g.io.partial = true;
        assert!(g.focus(&target(), deadline()).is_err());
        assert_eq!(g.io.submitted, 0);
    }
    #[test]
    fn failed_ack_or_readback_reports_possible_effect_and_poisoned_view() {
        for fail_submit in [true, false] {
            let mut g = guard();
            g.io.fail_submit = fail_submit;
            g.io.queries
                .extend([rows(), json!({"stableId":"18000001"})]);
            assert_eq!(
                g.focus(&target(), deadline()),
                Err(Error::EffectUnconfirmed)
            );
            assert_eq!(g.io.submitted, 1);
            assert!(!g.ready);
            assert_eq!(g.focus(&target(), deadline()), Err(Error::Unavailable));
            assert_eq!(g.io.submitted, 1);
        }
    }
    #[test]
    fn post_submit_close_and_expired_deadline_are_not_success() {
        let mut g = guard();
        g.io.queries
            .extend([rows(), json!({"stableId":"18000000"}), rows()]);
        g.io.events
            .extend([Ok(None), Ok(None), Ok(Some("closewindow".into()))]);
        assert_eq!(
            g.focus(&target(), deadline()),
            Err(Error::EffectUnconfirmed)
        );
        assert_eq!(g.io.submitted, 1);
        let mut g = guard();
        assert!(g.focus(&target(), Instant::now()).is_err());
        assert_eq!(g.io.submitted, 0);
    }
    #[test]
    fn snapshot_ambiguity_and_protocol_drift_are_refused() {
        let mut g = guard();
        g.io.queries.push_back(rows());
        g.io.events
            .extend([Ok(None), Ok(Some("openwindow".into())), Ok(None)]);
        assert!(g.snapshot(deadline()).is_err());
        assert!(!g.ready);
        let mut io = g.io;
        io.queries = VecDeque::from([json!({"commit":"different-version"})]);
        assert!(Guard::new(io, "instance".into(), deadline()).is_err());
    }
    #[test]
    fn installed_source_fixture_uses_exact_stable_selector_and_counter() {
        let query = include_str!("../fixtures/hyprland-0.56.2/ViewQuery.cpp");
        assert!(query.contains("stableid:"));
        assert!(query.contains("w->m_stableID"));
        let ctl = include_str!("../fixtures/hyprland-0.56.2/HyprCtl.cpp");
        assert!(ctl.contains("\"stableId\": \"{:x}\""));
        let window = include_str!("../fixtures/hyprland-0.56.2/Window.cpp");
        assert!(window.contains("m_stableID(windowIDCounter++)"));
    }
    #[test]
    fn surface_readiness_requires_pid_namespace_geometry_and_unambiguous_presence() {
        let layer = json!({"namespace":"fresh","pid":123,"w":100,"h":50,"alpha":1.});
        let layers = |row: Value| json!({"monitor":{"levels":{"0":[],"1":[],"2":[],"3":[row]}}});
        assert_eq!(
            surface_present(&layers(layer.clone()), 123, "fresh"),
            Ok(true)
        );
        assert_eq!(
            surface_present(&layers(layer.clone()), 124, "fresh"),
            Err(Error::Unavailable)
        );
        assert_eq!(
            surface_present(&layers(layer.clone()), 123, "old"),
            Ok(false)
        );
        let mut zero = layer.clone();
        zero["w"] = json!(0);
        assert!(surface_present(&layers(zero), 123, "fresh").is_err());
        assert!(
            surface_present(
                &json!({"monitor":{"levels":{"0":[],"1":[],"2":[],"3":[layer.clone(),layer.clone()]}}}),
                123,
                "fresh"
            )
            .is_err()
        );
        assert_eq!(namespace_absent(&layers(layer), "fresh"), Ok(false));
        assert_eq!(namespace_absent(&json!({}), "fresh"), Ok(true));
        assert!(namespace_absent(&json!({"monitor":{"levels":null}}), "fresh").is_err());
    }
    #[test]
    fn invisible_layer_waits_but_does_not_hide_invalid_identity_or_duplicates() {
        let row = json!({"namespace":"fresh","pid":123,"w":10,"h":10,"alpha":0});
        let layers = |rows: Value| json!({"monitor":{"levels":{"0":[],"1":[],"2":[],"3":rows}}});
        assert_eq!(
            surface_present(&layers(json!([row.clone()])), 123, "fresh"),
            Ok(false)
        );
        assert!(surface_present(&layers(json!([row.clone()])), 124, "fresh").is_err());
        assert!(surface_present(&layers(json!([row.clone(), row.clone()])), 123, "fresh").is_err());
        for alpha in [json!(-0.1), json!(1.1), json!(null), json!("0")] {
            let mut invalid = row.clone();
            invalid["alpha"] = alpha;
            assert!(surface_present(&layers(json!([invalid])), 123, "fresh").is_err());
        }
        let mut visible = row;
        visible["alpha"] = json!(0.01);
        assert_eq!(
            surface_present(&layers(json!([visible])), 123, "fresh"),
            Ok(true)
        );
    }
    #[test]
    fn incomplete_layer_snapshot_cannot_prove_readiness_or_dismissal() {
        let row = json!({"namespace":"fresh","pid":123,"w":10,"h":10,"alpha":1});
        for levels in [
            json!({}),
            json!({"3":[]}),
            json!({"0":[],"1":[],"2":[]}),
            json!({"0":[],"1":[],"2":[],"other":[]}),
            json!({"0":[],"1":[],"2":[],"3":[],"4":[]}),
        ] {
            let snapshot = json!({"monitor":{"levels":levels}});
            assert!(namespace_absent(&snapshot, "fresh").is_err(), "{snapshot}");
        }
        let partial = json!({"monitor":{"levels":{"3":[row]}}});
        assert!(surface_present(&partial, 123, "fresh").is_err());
        let complete = json!({"monitor":{"levels":{"0":[],"1":[],"2":[],"3":[]}}});
        assert_eq!(namespace_absent(&complete, "fresh"), Ok(true));
        assert_eq!(surface_present(&complete, 123, "fresh"), Ok(false));
    }

    #[test]
    fn submap_entry_switch_reset_are_closed_and_read_back() {
        use desktop_io::ModalSubmap::*;
        let mut g = guard();
        let mut owned = None;
        g.io.queries.extend([
            json!("default"),
            json!("vimarchy"),
            json!("vimarchy"),
            json!("vimarchy-double"),
            json!("vimarchy-double"),
            json!("default"),
        ]);
        g.transition(&mut owned, Vimarchy, deadline()).unwrap();
        g.transition(&mut owned, VimarchyDouble, deadline())
            .unwrap();
        g.transition(&mut owned, Reset, deadline()).unwrap();
        assert_eq!(g.io.submaps, [Vimarchy, VimarchyDouble, Reset]);
        assert!(owned.is_none());
        assert!(!g.uncertain);
    }
    #[test]
    fn foreign_submap_is_never_overwritten_and_uncertain_delivery_is_latched() {
        use desktop_io::ModalSubmap::*;
        let mut g = guard();
        let mut owned = Some(Vimarchy);
        g.io.queries.push_back(json!("user-other"));
        assert_eq!(
            g.transition(&mut owned, Reset, deadline()),
            Err(Error::Unavailable)
        );
        assert!(g.io.submaps.is_empty());
        g.io.queries.extend([json!("vimarchy"), json!("wrong")]);
        assert_eq!(
            g.transition(&mut owned, Reset, deadline()),
            Err(Error::EffectUnconfirmed)
        );
        assert!(g.uncertain);
        g.io.queries.push_back(json!("default"));
        assert_eq!(
            g.transition(&mut owned, Reset, deadline()),
            Err(Error::EffectUnconfirmed)
        );
        assert_eq!(g.io.submaps, [Reset]);
    }
    #[test]
    fn already_default_reset_does_not_mutate_and_clears_recorded_ownership() {
        let mut g = guard();
        let mut owned = Some(desktop_io::ModalSubmap::Ask);
        g.io.queries.push_back(json!("default"));
        g.transition(&mut owned, desktop_io::ModalSubmap::Reset, deadline())
            .unwrap();
        assert!(owned.is_none());
        assert!(g.io.submaps.is_empty());
    }
}
