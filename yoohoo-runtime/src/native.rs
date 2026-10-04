//! Explicit read-only adapter. Only authenticated socket events can create native attention.
use crate::{Error, Runtime};
use desktop_io::{
    Address as DesktopAddress, Endpoint, Events, Identity, Query, Snapshots, StableId,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use yoohoo_core::{
    identity::{Address, WindowKey},
    reducer::{Client, Event, Health, Source, SourceHealth},
};
pub struct Native {
    endpoint: Arc<Endpoint>,
    events: Events,
    snapshots: Snapshots,
    identities: BTreeMap<String, Identity>,
    stable_ids: BTreeMap<String, StableId>,
}
impl Native {
    pub fn connect(endpoint: Endpoint, epoch: [u8; 16]) -> Result<Self, desktop_io::Error> {
        Self::connect_before(
            Arc::new(endpoint),
            epoch,
            Instant::now() + Duration::from_millis(500),
        )
    }
    pub(crate) fn connect_before(
        endpoint: Arc<Endpoint>,
        epoch: [u8; 16],
        deadline: Instant,
    ) -> Result<Self, desktop_io::Error> {
        let budget = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(desktop_io::Error::Deadline)?;
        let events = endpoint.events(budget)?;
        Ok(Self {
            endpoint,
            events,
            snapshots: Snapshots::new(epoch)?,
            identities: BTreeMap::new(),
            stable_ids: BTreeMap::new(),
        })
    }
    pub fn synchronize(&mut self, runtime: &mut Runtime, now: u64) -> Result<(), Error> {
        self.synchronize_before(runtime, now, Instant::now() + Duration::from_secs(1))
    }
    pub(crate) fn synchronize_before(
        &mut self,
        runtime: &mut Runtime,
        now: u64,
        deadline: Instant,
    ) -> Result<(), Error> {
        let result = self.sync_inner(runtime, now, deadline);
        if result.is_err() {
            self.snapshots.invalidate();
            self.identities.clear();
            self.stable_ids.clear();
            runtime.attention.disconnected()?;
            runtime.pending = None;
        }
        result
    }
    fn sync_inner(
        &mut self,
        runtime: &mut Runtime,
        now: u64,
        deadline: Instant,
    ) -> Result<(), Error> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or(Error::Stale)
        };
        let rows = self
            .snapshots
            .collect_clients(&self.endpoint, &mut self.events, remaining()?)
            .map_err(|_| Error::Stale)?;
        let active = self
            .endpoint
            .query(Query::ActiveWindow, remaining()?)
            .map_err(|_| Error::Stale)?;
        // Refuse a second-query race instead of mixing focused state with another revision.
        if self
            .snapshots
            .poll(&mut self.events)
            .map_err(|_| Error::Stale)?
            .is_some()
            || self.events.has_partial()
        {
            return Err(Error::Stale);
        }
        let rows = rows.as_array().ok_or(Error::Invalid)?;
        let mut clients = Vec::new();
        let mut identities = BTreeMap::new();
        let mut stable_ids = BTreeMap::new();
        let mut seen_stable = std::collections::BTreeSet::new();
        for row in rows {
            let address = DesktopAddress::parse(
                row.get("address")
                    .and_then(|v| v.as_str())
                    .ok_or(Error::Invalid)?,
            )
            .map_err(|_| Error::Invalid)?;
            let identity = self.snapshots.identity(address).ok_or(Error::Stale)?;
            if let Some(raw) = row.get("stableId").and_then(|v| v.as_str()) {
                let stable = StableId::parse(raw).map_err(|_| Error::Invalid)?;
                if !seen_stable.insert(stable.canonical()) {
                    return Err(Error::Invalid);
                }
                stable_ids.insert(address.canonical(), stable);
            }
            let epoch = identity
                .process_epoch
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let stable = format!("{epoch}:{}:{}", identity.connection, identity.generation);
            let key = WindowKey::new(
                &identity.instance,
                Address::parse(&address.canonical())?,
                &stable,
            )?;
            let text = |field: &str| {
                row.get(field)
                    .and_then(|v| v.as_str())
                    .filter(|v| v.len() <= 4096)
                    .map(str::to_owned)
                    .ok_or(Error::Invalid)
            };
            let workspace = row.get("workspace").ok_or(Error::Invalid)?;
            let client = Client {
                key: key.clone(),
                process: None,
                title: text("title")?,
                class: text("class")?,
                workspace: workspace
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|v| v.len() <= 4096)
                    .ok_or(Error::Invalid)?
                    .into(),
                workspace_id: workspace
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or(Error::Invalid)?,
            };
            client.validate()?;
            identities.insert(address.canonical(), identity);
            clients.push(client);
        }
        // Hyprland represents no active window as an empty object. Missing or
        // mistyped address in any other response is unknown state, not no focus.
        let active = active.as_object().ok_or(Error::Invalid)?;
        let address = if active.is_empty() {
            None
        } else {
            Some(
                active
                    .get("address")
                    .and_then(|v| v.as_str())
                    .ok_or(Error::Invalid)?,
            )
        };
        let focused = match address {
            None | Some("0x0") => None,
            Some(raw) => {
                let address = DesktopAddress::parse(raw)
                    .map_err(|_| Error::Invalid)?
                    .canonical();
                Some(
                    clients
                        .iter()
                        .find(|c| c.key.address().as_str() == address)
                        .ok_or(Error::Stale)?
                        .key
                        .clone(),
                )
            }
        };
        let instance = clients
            .first()
            .map(|c| c.key.instance().to_owned())
            .or_else(|| identities.values().next().map(|i| i.instance.clone()))
            .unwrap_or_else(|| "empty-native".into());
        remaining()?;
        let effects = runtime.attention.resnapshot(
            &instance,
            runtime.sequence,
            clients,
            focused,
            &[],
            now,
        )?;
        runtime.instance = instance;
        runtime.stage(effects)?;
        runtime.pending = None;
        runtime.attention.set_health(SourceHealth {
            native: Health::Ready,
            notifications: Health::Unavailable,
            audio: Health::Unavailable,
            storage: Health::Unavailable,
        })?;
        runtime.refresh_selection()?;
        self.identities = identities;
        self.stable_ids = stable_ids;
        Ok(())
    }
    /// Source: window-attention.refresh_window. Metadata never creates attention or
    /// changes its age/count/source. A query cannot silently replace tracker identity.
    fn refresh_metadata(
        &mut self,
        runtime: &mut Runtime,
        raw: &str,
        now: u64,
        deadline: Instant,
    ) -> Result<(), Error> {
        let parse = |payload: &str| {
            DesktopAddress::parse(payload.split(',').next().unwrap_or(""))
                .map(|a| a.canonical())
                .map_err(|_| Error::Invalid)
        };
        let mut wanted = std::collections::BTreeSet::from([parse(raw)?]);
        // Hyprland may emit both title variants for one change. Coalesce only
        // metadata events already queued before the query; do not swallow a
        // lifecycle/focus/attention race or guess its ordering against the reply.
        let mut drained = false;
        for _ in 0..32 {
            if Instant::now() >= deadline {
                return Err(Error::Stale);
            }
            match self
                .snapshots
                .poll(&mut self.events)
                .map_err(|_| Error::Stale)?
            {
                None if !self.events.has_partial() => {
                    drained = true;
                    break;
                }
                Some((event, desktop_io::EventOutcome::Applied))
                    if matches!(
                        event.name.as_str(),
                        "windowtitle" | "windowtitlev2" | "movewindow" | "movewindowv2"
                    ) =>
                {
                    wanted.insert(parse(&event.payload)?);
                }
                _ => return Err(Error::Stale),
            }
        }
        if !drained {
            return Err(Error::Exhausted);
        }
        let rows = self
            .endpoint
            .query(
                Query::Clients,
                deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(Error::Stale)?,
            )
            .map_err(|_| Error::Stale)?;
        if self
            .snapshots
            .poll(&mut self.events)
            .map_err(|_| Error::Stale)?
            .is_some()
            || self.events.has_partial()
        {
            return Err(Error::Stale);
        }
        let rows = rows.as_array().ok_or(Error::Invalid)?;
        if rows.len() > desktop_io::MAX_CLIENTS {
            return Err(Error::Invalid);
        }
        let mut addresses = std::collections::BTreeSet::new();
        let mut stable = std::collections::BTreeSet::new();
        let mut updates = Vec::new();
        for row in rows {
            if Instant::now() >= deadline {
                return Err(Error::Stale);
            }
            let address = DesktopAddress::parse(
                row.get("address")
                    .and_then(|x| x.as_str())
                    .ok_or(Error::Invalid)?,
            )
            .map_err(|_| Error::Invalid)?;
            if !addresses.insert(address.canonical()) {
                return Err(Error::Invalid);
            }
            let stable_id = StableId::parse(
                row.get("stableId")
                    .and_then(|x| x.as_str())
                    .ok_or(Error::Invalid)?,
            )
            .map_err(|_| Error::Invalid)?;
            if !stable.insert(stable_id.canonical()) {
                return Err(Error::Invalid);
            }
            if !wanted.contains(&address.canonical()) {
                continue;
            }
            let identity = self
                .identities
                .get(&address.canonical())
                .ok_or(Error::Stale)?;
            if !self.snapshots.is_current(identity)
                || self.stable_ids.get(&address.canonical()) != Some(&stable_id)
            {
                return Err(Error::Stale);
            }
            let epoch = identity
                .process_epoch
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let key = WindowKey::new(
                &identity.instance,
                Address::parse(&address.canonical())?,
                &format!("{epoch}:{}:{}", identity.connection, identity.generation),
            )?;
            let text = |field: &str| {
                row.get(field)
                    .and_then(|v| v.as_str())
                    .filter(|v| v.len() <= 4096)
                    .map(str::to_owned)
                    .ok_or(Error::Invalid)
            };
            let workspace = row.get("workspace").ok_or(Error::Invalid)?;
            let client = Client {
                key,
                process: None,
                title: text("title")?,
                class: text("class")?,
                workspace: workspace
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|v| v.len() <= 4096)
                    .ok_or(Error::Invalid)?
                    .into(),
                workspace_id: workspace
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or(Error::Invalid)?,
            };
            client.validate()?;
            updates.push(client);
        }
        if updates.len() != wanted.len() {
            return Err(Error::Stale);
        }
        for client in updates {
            runtime.apply(Event::Refresh(client), now)?;
        }
        Ok(())
    }
    pub fn poll(&mut self, runtime: &mut Runtime, now: u64) -> Result<bool, Error> {
        self.poll_before(runtime, now, Instant::now() + Duration::from_millis(500))
    }
    pub(crate) fn poll_before(
        &mut self,
        runtime: &mut Runtime,
        now: u64,
        deadline: Instant,
    ) -> Result<bool, Error> {
        self.poll_with_completion_clock(runtime, now, deadline, Instant::now)
    }
    fn poll_with_completion_clock(
        &mut self,
        runtime: &mut Runtime,
        now: u64,
        deadline: Instant,
        completed: impl FnOnce() -> Instant,
    ) -> Result<bool, Error> {
        let result = if Instant::now() >= deadline {
            Err(Error::Stale)
        } else {
            self.poll_inner(runtime, now, deadline)
        };
        // A successful idle or event result arriving at/after the caller's
        // deadline is stale too; use exactly the same revocation path.
        let result = if completed() >= deadline {
            Err(Error::Stale)
        } else {
            result
        };
        if result.is_err() {
            self.snapshots.invalidate();
            self.identities.clear();
            self.stable_ids.clear();
            runtime.attention.disconnected()?;
            runtime.pending = None;
        }
        result
    }
    fn poll_inner(
        &mut self,
        runtime: &mut Runtime,
        now: u64,
        deadline: Instant,
    ) -> Result<bool, Error> {
        let Some((event, outcome)) = self
            .snapshots
            .poll(&mut self.events)
            .map_err(|_| Error::Stale)?
        else {
            if self.events.has_partial() {
                return Err(Error::Stale);
            }
            return Ok(false);
        };
        if outcome == desktop_io::EventOutcome::ResnapshotRequired {
            return Err(Error::Stale);
        }
        let key = |raw: &str| -> Result<WindowKey, Error> {
            let address = DesktopAddress::parse(raw)
                .map_err(|_| Error::Invalid)?
                .canonical();
            let identity = self.identities.get(&address).ok_or(Error::Stale)?;
            let epoch = identity
                .process_epoch
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            Ok(WindowKey::new(
                &identity.instance,
                Address::parse(&address)?,
                &format!("{epoch}:{}:{}", identity.connection, identity.generation),
            )?)
        };
        match event.name.as_str() {
            "urgent" => {
                let k = key(&event.payload)?;
                let identity = self
                    .identities
                    .get(k.address().as_str())
                    .ok_or(Error::Stale)?;
                if !self.snapshots.is_current(identity) {
                    return Err(Error::Stale);
                }
                runtime.apply(
                    Event::Attention {
                        key: k,
                        source: Source::Native,
                    },
                    now,
                )?;
            }
            "activewindowv2" => {
                let focus = if event.payload.is_empty() {
                    None
                } else {
                    Some(key(&event.payload)?)
                };
                runtime.apply(Event::Focus(focus), now)?;
            }
            "closewindow" => {
                let k = key(&event.payload)?;
                self.identities.remove(k.address().as_str());
                self.stable_ids.remove(k.address().as_str());
                runtime.apply(Event::Close(k), now)?;
            }
            "windowtitle" | "windowtitlev2" | "movewindow" | "movewindowv2" => {
                self.refresh_metadata(runtime, &event.payload, now, deadline)?;
            }
            "openwindow" => return Err(Error::Stale),
            _ => {}
        }
        Ok(true)
    }
    /// Metadata join only: local tracker identity never grants target authority.
    pub fn stable_target(&self, key: &WindowKey) -> Option<(&str, String)> {
        let identity = self.observation(key)?;
        let id = self.stable_ids.get(key.address().as_str())?;
        Some((&identity.instance, id.canonical()))
    }
    pub fn observation(&self, key: &WindowKey) -> Option<&Identity> {
        self.identities.get(key.address().as_str()).filter(|i| {
            let epoch = i
                .process_epoch
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            self.snapshots.is_current(i)
                && key.instance() == i.instance
                && key.stable_id() == format!("{epoch}:{}:{}", i.connection, i.generation)
        })
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        thread,
    };
    fn one(listener: &UnixListener, reply: &[u8]) {
        let (mut peer, _) = listener.accept().unwrap();
        let mut request = [0; 64];
        assert!(peer.read(&mut request).unwrap() > 0);
        peer.write_all(reply).unwrap();
    }
    fn scenario(fault: &str) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(dir.path().join("hypr/test")).unwrap();
        let listener = UnixListener::bind(dir.path().join("hypr/test/.socket.sock")).unwrap();
        let event = UnixListener::bind(dir.path().join("hypr/test/.socket2.sock")).unwrap();
        let endpoint = Endpoint::discover(dir.path(), "test", std::process::id()).unwrap();
        let mut native = Native::connect(endpoint, [9; 16]).unwrap();
        let (mut events, _) = event.accept().unwrap();
        let row = serde_json::json!({"address":"0x1","stableId":"a","title":"Old","class":"Terminal","workspace":{"id":1,"name":"One"}});
        let initial = serde_json::to_vec(&vec![row.clone()]).unwrap();
        let init = thread::spawn(move || {
            one(&listener, &initial);
            one(&listener, b"{}");
            listener
        });
        let mut runtime = Runtime::empty().unwrap();
        native.synchronize(&mut runtime, 1).unwrap();
        let listener = init.join().unwrap();
        events.write_all(b"urgent>>0x1\n").unwrap();
        native.poll(&mut runtime, 2).unwrap();
        events.write_all(b"urgent>>0x1\n").unwrap();
        native.poll(&mut runtime, 3).unwrap();
        runtime.execute(crate::Command::Open, 4).unwrap();
        let before = runtime.attention.snapshot(true).windows.remove(0);
        let identity = native.observation(&before.key).unwrap().clone();
        if fault == "late-idle" {
            let view = runtime.view();
            runtime
                .execute(
                    crate::Command::Activate {
                        generation: view.generation,
                        revision: view.revision,
                        id: view.rows[0].id,
                    },
                    4,
                )
                .unwrap();
            assert!(runtime.pending.is_some());
            let end = Instant::now() + Duration::from_secs(1);
            let result = native.poll_with_completion_clock(&mut runtime, 5, end, || end);
            assert!(result.is_err(), "late idle result accepted");
            assert!(runtime.view().stale);
            assert!(runtime.pending.is_none());
            assert!(native.observation(&before.key).is_none());
            assert!(native.stable_target(&before.key).is_none());
            return;
        }
        let mut changed = row;
        changed["title"] = "New 日本語".into();
        changed["workspace"] = serde_json::json!({"id":2,"name":"Two"});
        if fault == "reuse" {
            changed["stableId"] = "b".into();
        }
        let rows = if fault == "duplicate" {
            vec![changed.clone(), changed]
        } else {
            vec![changed]
        };
        let bytes = serde_json::to_vec(&rows).unwrap();
        let mut race: UnixStream = events.try_clone().unwrap();
        let inject = fault == "race";
        let slow = fault == "deadline";
        let query = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut request = [0; 64];
            assert!(peer.read(&mut request).unwrap() > 0);
            if inject {
                race.write_all(b"closewindow>>0x1\nopenwindow>>0x1,2,Terminal,Replacement\n")
                    .unwrap();
            }
            if slow {
                thread::sleep(Duration::from_millis(120));
            }
            let _ = peer.write_all(&bytes);
        });
        events
            .write_all(b"windowtitle>>0x1\nwindowtitlev2>>0x1,new\nmovewindowv2>>0x1,2,Two\n")
            .unwrap();
        let started = Instant::now();
        let result = native.poll_before(
            &mut runtime,
            5,
            started + Duration::from_millis(if slow { 20 } else { 500 }),
        );
        if slow {
            assert!(started.elapsed() < Duration::from_millis(100));
        }
        query.join().unwrap();
        if fault.is_empty() {
            assert!(result.unwrap());
            let after = runtime.attention.snapshot(true).windows.remove(0);
            assert_eq!(after.key, before.key);
            assert_eq!(after.opaque_id, before.opaque_id);
            assert_eq!(after.count, before.count);
            assert_eq!(after.sources, before.sources);
            assert_eq!(after.first_attention_at_ms, before.first_attention_at_ms);
            assert_eq!(after.last_attention_at_ms, before.last_attention_at_ms);
            assert_eq!(after.title.as_deref(), Some("New 日本語"));
            assert_eq!(after.workspace.as_deref(), Some("Two"));
            assert_eq!(native.observation(&after.key), Some(&identity));
            assert!(runtime.view().rows[0].selected);
            assert!(!runtime.view().pending_activation);
        } else {
            assert!(result.is_err());
            assert!(runtime.view().stale);
            assert!(native.observation(&before.key).is_none());
            assert!(!runtime.view().pending_activation);
        }
    }
    #[test]
    fn metadata_preserves_full_identity_age_count_source_and_selection() {
        scenario("");
    }
    #[test]
    fn same_address_different_stable_id_revokes() {
        scenario("reuse");
    }
    #[test]
    fn lifecycle_during_metadata_query_revokes() {
        scenario("race");
    }
    #[test]
    fn duplicate_snapshot_address_revokes() {
        scenario("duplicate");
    }
    #[test]
    fn late_idle_completion_revokes_identity_and_pending_activation() {
        scenario("late-idle");
    }
    #[test]
    fn caller_query_deadline_is_not_restarted() {
        scenario("deadline");
    }
}
