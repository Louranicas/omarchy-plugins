use crate::{
    Error,
    config::{Config, MAX_METADATA_BYTES, SoundGate},
    history::{History, HistoryEvent, Record},
    identity::{ProcessKey, WindowKey},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq, Eq)]
pub struct Client {
    pub key: WindowKey,
    pub process: Option<ProcessKey>,
    pub title: String,
    pub class: String,
    pub workspace: String,
    pub workspace_id: i64,
}
impl Client {
    pub fn validate(&self) -> Result<(), Error> {
        if [&self.title, &self.class, &self.workspace]
            .iter()
            .any(|s| s.len() > MAX_METADATA_BYTES)
            || self
                .process
                .is_some_and(|p| p.pid == 0 || p.start_ticks == 0)
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Client(<redacted>)")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Native,
    Notification,
}
impl Source {
    fn bit(self) -> u8 {
        match self {
            Self::Native => 1,
            Self::Notification => 2,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Unknown,
    Ready,
    Degraded,
    Unavailable,
    Disabled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SourceHealth {
    pub native: Health,
    pub notifications: Health,
    pub audio: Health,
    pub storage: Health,
}
impl Default for SourceHealth {
    fn default() -> Self {
        Self {
            native: Health::Unknown,
            notifications: Health::Unknown,
            audio: Health::Unknown,
            storage: Health::Unknown,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SetAttention(WindowKey),
    ClearOwnedTags(WindowKey),
    PlaySound { operation: u64 },
    StopSound { operation: u64 },
    Publish { revision: u64 },
}
#[derive(Debug, Clone)]
pub enum Event {
    Attention { key: WindowKey, source: Source },
    Focus(Option<WindowKey>),
    Close(WindowKey),
    Refresh(Client),
}
#[derive(Debug, Clone)]
pub struct Envelope {
    pub instance: String,
    pub sequence: u64,
    pub event: Event,
    pub monotonic_ms: u64,
    pub wall_ms: i64,
}
#[derive(Clone)]
struct Pending {
    client: Client,
    first: i64,
    last: i64,
    last_mono: u64,
    count: u64,
    sources: u8,
    opaque_id: u64,
}
#[derive(Serialize)]
pub struct EntryView {
    pub key: WindowKey,
    pub first_attention_at_ms: i64,
    pub last_attention_at_ms: i64,
    pub count: u64,
    pub sources: u8,
    pub opaque_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<i64>,
}
#[derive(Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub revision: u64,
    pub stale: bool,
    pub health: SourceHealth,
    pub windows: Vec<EntryView>,
}

pub struct Attention {
    config: Config,
    instance: Option<String>,
    sequence: u64,
    revision: u64,
    time: u64,
    focused: Option<WindowKey>,
    live: BTreeMap<WindowKey, Client>,
    pending: BTreeMap<WindowKey, Pending>,
    next_opaque: u64,
    synchronized: bool,
    stopped: bool,
    health: SourceHealth,
    sound: SoundGate,
    history: History,
}
impl Attention {
    pub fn new(config: Config) -> Result<Self, Error> {
        config.validate()?;
        Ok(Self {
            config,
            instance: None,
            sequence: 0,
            revision: 0,
            time: 0,
            focused: None,
            live: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_opaque: 1,
            synchronized: false,
            stopped: false,
            health: SourceHealth::default(),
            sound: SoundGate::default(),
            history: History::default(),
        })
    }
    fn advance(&mut self) -> Result<(), Error> {
        self.revision = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(())
    }
    fn check_time(&self, now: u64) -> Result<(), Error> {
        if now < self.time {
            Err(Error::ClockWentBackwards)
        } else {
            Ok(())
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn history(&self) -> &History {
        &self.history
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn purge_history(&mut self) {
        self.history.purge();
    }
    pub fn set_health(&mut self, health: SourceHealth) -> Result<(), Error> {
        self.advance()?;
        self.health = health;
        Ok(())
    }
    pub fn reload(&mut self, config: Config, now: u64) -> Result<(), Error> {
        config.validate()?;
        self.check_time(now)?;
        if self.live.len() > config.max_windows {
            return Err(Error::LimitExceeded);
        }
        self.advance()?;
        self.history.expire(now, &config)?;
        self.config = config;
        self.time = now;
        Ok(())
    }
    /// Complete authoritative snapshot/watermark. Only same-instance exact identities survive.
    /// `tagged` includes only keys carrying the two plugin-owned tags, never arbitrary tags.
    pub fn resnapshot(
        &mut self,
        instance: &str,
        sequence: u64,
        clients: Vec<Client>,
        focused: Option<WindowKey>,
        tagged: &[WindowKey],
        now: u64,
    ) -> Result<Vec<Effect>, Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.check_time(now)?;
        if instance.is_empty() || instance.len() > 128 || instance.chars().any(char::is_control) {
            return Err(Error::InvalidIdentity);
        }
        if clients.len() > self.config.max_windows || tagged.len() > self.config.max_windows {
            return Err(Error::LimitExceeded);
        }
        if self.instance.as_deref() == Some(instance) && sequence < self.sequence {
            return Err(Error::StaleRevision);
        }
        let mut live = BTreeMap::new();
        let mut addresses = BTreeSet::new();
        for client in clients {
            client.validate()?;
            if client.key.instance() != instance
                || !addresses.insert(client.key.address().clone())
                || live.insert(client.key.clone(), client).is_some()
            {
                return Err(Error::InvalidIdentity);
            }
        }
        if focused.as_ref().is_some_and(|key| !live.contains_key(key)) {
            return Err(Error::StaleIdentity);
        }
        self.advance()?;
        if self.instance.as_deref() != Some(instance) {
            self.pending.clear();
        }
        self.pending
            .retain(|key, _| live.contains_key(key) && focused.as_ref() != Some(key));
        for (key, pending) in &mut self.pending {
            pending.client = live[key].clone();
        }
        let mut effects: Vec<_> = self
            .pending
            .keys()
            .cloned()
            .map(Effect::SetAttention)
            .collect();
        for key in tagged.iter().collect::<BTreeSet<_>>() {
            if live.contains_key(key) && !self.pending.contains_key(key) {
                effects.push(Effect::ClearOwnedTags(key.clone()));
            }
        }
        self.live = live;
        self.focused = focused;
        self.instance = Some(instance.into());
        self.sequence = sequence;
        self.synchronized = true;
        self.health.native = Health::Ready;
        self.time = now;
        effects.push(Effect::Publish {
            revision: self.revision,
        });
        Ok(effects)
    }
    pub fn disconnected(&mut self) -> Result<(), Error> {
        self.advance()?;
        self.synchronized = false;
        self.health.native = Health::Unavailable;
        Ok(())
    }
    /// Distinct delivered signals increment the raw count. Exact sequence replays do not.
    pub fn apply(&mut self, e: Envelope) -> Result<Vec<Effect>, Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if !self.synchronized {
            return Err(Error::SourceUnavailable);
        }
        if self.instance.as_deref() != Some(e.instance.as_str()) {
            return Err(Error::StaleIdentity);
        }
        self.check_time(e.monotonic_ms)?;
        if e.sequence <= self.sequence {
            return Err(Error::Duplicate);
        }
        if self.sequence.checked_add(1) != Some(e.sequence) {
            self.disconnected()?;
            return Err(Error::SequenceGap);
        }
        // Validate all fallible event inputs before committing sequence or state.
        match &e.event {
            Event::Attention { key, source } => {
                if !self.live.contains_key(key) {
                    return Err(Error::StaleIdentity);
                }
                if *source == Source::Notification && self.health.notifications != Health::Ready {
                    return Err(Error::SourceUnavailable);
                }
                if self.pending.get(key).is_some_and(|p| p.count == u64::MAX)
                    || self.next_opaque == u64::MAX
                {
                    return Err(Error::Exhausted);
                }
            }
            Event::Focus(Some(key)) | Event::Close(key) => {
                if !self.live.contains_key(key) {
                    return Err(Error::StaleIdentity);
                }
            }
            Event::Refresh(c) => {
                c.validate()?;
                if !self.live.contains_key(&c.key) {
                    return Err(Error::StaleIdentity);
                }
            }
            Event::Focus(None) => {}
        }
        self.advance()?;
        self.sequence = e.sequence;
        self.time = e.monotonic_ms;
        let mut effects = Vec::new();
        match e.event {
            Event::Attention { key, source } => {
                if self.focused.as_ref() != Some(&key) {
                    let fresh = !self.pending.contains_key(&key);
                    let p = self.pending.entry(key.clone()).or_insert_with(|| {
                        let id = self.next_opaque;
                        self.next_opaque += 1;
                        Pending {
                            client: self.live[&key].clone(),
                            first: e.wall_ms,
                            last: e.wall_ms,
                            last_mono: e.monotonic_ms,
                            count: 0,
                            sources: 0,
                            opaque_id: id,
                        }
                    });
                    p.count += 1;
                    p.last = e.wall_ms;
                    p.last_mono = e.monotonic_ms;
                    p.sources |= source.bit();
                    let record = Record {
                        schema_version: 1,
                        opaque_id: p.opaque_id,
                        at_ms: e.wall_ms,
                        event: HistoryEvent::Attention,
                        count: p.count,
                        source_bits: p.sources,
                    };
                    self.record(record, e.monotonic_ms);
                    effects.push(Effect::SetAttention(key));
                    if fresh
                        && let Some(operation) = self.sound.request(e.monotonic_ms, &self.config)
                    {
                        effects.push(Effect::PlaySound { operation });
                    }
                }
            }
            Event::Focus(key) => {
                self.focused = key.clone();
                if let Some(key) = key {
                    self.remove(
                        &key,
                        HistoryEvent::Focused,
                        e.wall_ms,
                        e.monotonic_ms,
                        true,
                        &mut effects,
                    );
                }
            }
            Event::Close(key) => {
                self.live.remove(&key);
                if self.focused.as_ref() == Some(&key) {
                    self.focused = None;
                }
                self.remove(
                    &key,
                    HistoryEvent::Closed,
                    e.wall_ms,
                    e.monotonic_ms,
                    false,
                    &mut effects,
                );
            }
            Event::Refresh(client) => {
                if let Some(p) = self.pending.get_mut(&client.key) {
                    p.client = client.clone();
                }
                self.live.insert(client.key.clone(), client);
            }
        }
        effects.push(Effect::Publish {
            revision: self.revision,
        });
        Ok(effects)
    }
    fn record(&mut self, record: Record, now: u64) {
        if self.history.record(record, now, &self.config).is_err() {
            self.health.storage = Health::Degraded;
        }
    }
    fn remove(
        &mut self,
        key: &WindowKey,
        event: HistoryEvent,
        wall: i64,
        now: u64,
        tags: bool,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(p) = self.pending.remove(key) {
            self.record(
                Record {
                    schema_version: 1,
                    opaque_id: p.opaque_id,
                    at_ms: wall,
                    event,
                    count: p.count,
                    source_bits: p.sources,
                },
                now,
            );
            if tags {
                effects.push(Effect::ClearOwnedTags(key.clone()));
            }
        }
    }
    /// Last in-core check only; adapter must requery identity again immediately before dispatch.
    /// Returning a key does not confer focus authority or imply exactly-once delivery.
    pub fn activation_target(
        &self,
        key: &WindowKey,
        displayed_revision: u64,
    ) -> Result<WindowKey, Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if !self.synchronized {
            return Err(Error::SourceUnavailable);
        }
        if displayed_revision != self.revision {
            return Err(Error::StaleRevision);
        }
        if !self.live.contains_key(key) || !self.pending.contains_key(key) {
            return Err(Error::StaleIdentity);
        }
        Ok(key.clone())
    }
    pub fn clear(&mut self, key: &WindowKey, wall: i64, now: u64) -> Result<Vec<Effect>, Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if !self.synchronized {
            return Err(Error::SourceUnavailable);
        }
        self.check_time(now)?;
        if !self.live.contains_key(key) {
            return Err(Error::StaleIdentity);
        }
        self.advance()?;
        self.time = now;
        let mut effects = Vec::new();
        self.remove(key, HistoryEvent::Cleared, wall, now, true, &mut effects);
        effects.push(Effect::Publish {
            revision: self.revision,
        });
        Ok(effects)
    }
    pub fn audio_finished(&mut self, operation: u64, success: bool) -> Result<Vec<Effect>, Error> {
        if self.sound.playing() != Some(operation) {
            return Err(Error::StaleIdentity);
        }
        self.advance()?;
        self.sound.finished(operation)?;
        self.health.audio = if success {
            Health::Ready
        } else {
            Health::Degraded
        };
        Ok(vec![Effect::Publish {
            revision: self.revision,
        }])
    }
    pub fn tick(&mut self, now: u64) -> Result<(), Error> {
        self.check_time(now)?;
        self.history.expire(now, &self.config)?;
        self.time = now;
        Ok(())
    }
    pub fn shutdown(&mut self, wall: i64, now: u64) -> Result<Vec<Effect>, Error> {
        self.check_time(now)?;
        if self.stopped {
            return Ok(Vec::new());
        }
        self.advance()?;
        let mut effects = Vec::new();
        let keys: Vec<_> = self.pending.keys().cloned().collect();
        for key in keys {
            self.remove(
                &key,
                HistoryEvent::ServiceStopped,
                wall,
                now,
                self.synchronized && self.live.contains_key(&key),
                &mut effects,
            );
        }
        if let Some(operation) = self.sound.playing() {
            effects.push(Effect::StopSound { operation });
            self.sound.finished(operation)?;
        }
        self.stopped = true;
        self.synchronized = false;
        self.time = now;
        effects.push(Effect::Publish {
            revision: self.revision,
        });
        Ok(effects)
    }
    pub fn snapshot(&self, include_metadata: bool) -> Snapshot {
        let mut rows: Vec<_> = self.pending.values().collect();
        // Monotonic ordering survives wall-clock corrections; exact ties use stable identity.
        rows.sort_by(|a, b| {
            b.last_mono
                .cmp(&a.last_mono)
                .then_with(|| a.client.key.cmp(&b.client.key))
        });
        Snapshot {
            schema_version: 1,
            revision: self.revision,
            stale: !self.synchronized,
            health: self.health,
            windows: rows
                .into_iter()
                .map(|p| EntryView {
                    key: p.client.key.clone(),
                    first_attention_at_ms: p.first,
                    last_attention_at_ms: p.last,
                    count: p.count,
                    sources: p.sources,
                    opaque_id: p.opaque_id,
                    title: include_metadata.then(|| p.client.title.clone()),
                    class: include_metadata.then(|| p.client.class.clone()),
                    workspace: include_metadata.then(|| p.client.workspace.clone()),
                    workspace_id: include_metadata.then_some(p.client.workspace_id),
                })
                .collect(),
        }
    }
}
/// Called with bus-verified credentials + current proc identity by the adapter. Never parse
/// summary/body/action text for attribution. Ambiguous processes remain unresolved.
pub fn unique_process_window(process: ProcessKey, clients: &[Client]) -> Option<WindowKey> {
    if process.pid == 0 || process.start_ticks == 0 {
        return None;
    }
    let mut matches = clients.iter().filter(|c| c.process == Some(process));
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first.key.clone())
    }
}
