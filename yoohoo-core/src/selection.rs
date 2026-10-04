use crate::{
    Error,
    config::{MAX_STREAMS, MAX_WINDOWS, SEQUENCE_WINDOW},
    identity::WindowKey,
};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
}
/// Source: payload/Selection.js `step`; address comparisons upgraded to full identity.
pub fn step(
    windows: &[WindowKey],
    selected: Option<&WindowKey>,
    direction: Direction,
) -> Option<WindowKey> {
    if windows.is_empty() {
        return None;
    }
    let index = selected.and_then(|s| windows.iter().position(|w| w == s));
    let next = match (index, direction) {
        (None, Direction::Next) => 0,
        (None, Direction::Previous) => windows.len() - 1,
        (Some(i), Direction::Next) => (i + 1) % windows.len(),
        (Some(i), Direction::Previous) => (i + windows.len() - 1) % windows.len(),
    };
    Some(windows[next].clone())
}
/// Source: Selection.js `reconcile`: chosen survivor, next old survivor, then first.
pub fn reconcile(
    previous: &[WindowKey],
    windows: &[WindowKey],
    selected: Option<&WindowKey>,
) -> Option<WindowKey> {
    if let Some(selected) = selected {
        if windows.contains(selected) {
            return Some(selected.clone());
        }
        if let Some(index) = previous.iter().position(|w| w == selected) {
            for offset in 1..previous.len() {
                let candidate = &previous[(index + offset) % previous.len()];
                if windows.contains(candidate) {
                    return Some(candidate.clone());
                }
            }
        }
    }
    windows.first().cloned()
}
#[derive(Default, Debug)]
pub struct Selection {
    generation: u64,
    opened: bool,
    cycling: bool,
    loaded: bool,
    windows: Vec<WindowKey>,
    selected: Option<WindowKey>,
    queued: VecDeque<Direction>,
    revision: u64,
}
impl Selection {
    pub fn open(&mut self) -> Result<u64, Error> {
        self.generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        self.opened = true;
        self.loaded = false;
        self.cycling = false;
        self.windows.clear();
        self.selected = None;
        self.queued.clear();
        Ok(self.generation)
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    fn check_generation(&self, generation: u64) -> Result<(), Error> {
        if generation != self.generation {
            Err(Error::StaleRevision)
        } else {
            Ok(())
        }
    }
    pub fn close(&mut self, generation: u64) -> Result<(), Error> {
        self.check_generation(generation)?;
        *self = Self {
            generation: self.generation,
            ..Self::default()
        };
        Ok(())
    }
    pub fn is_open(&self) -> bool {
        self.opened
    }
    pub fn selected(&self) -> Option<&WindowKey> {
        self.selected.as_ref()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn update(
        &mut self,
        generation: u64,
        windows: Vec<WindowKey>,
        revision: u64,
    ) -> Result<(), Error> {
        self.check_generation(generation)?;
        if !self.opened {
            return Err(Error::NotOpen);
        }
        if windows.len() > MAX_WINDOWS {
            return Err(Error::LimitExceeded);
        }
        let unique: std::collections::BTreeSet<_> = windows.iter().collect();
        if unique.len() != windows.len() {
            return Err(Error::InvalidInput);
        }
        if self.loaded && revision < self.revision {
            return Err(Error::StaleRevision);
        }
        let selected = reconcile(&self.windows, &windows, self.selected.as_ref());
        self.windows = windows;
        self.selected = selected;
        self.revision = revision;
        self.loaded = true;
        if !self.queued.is_empty() {
            self.selected = None;
        }
        while let Some(dir) = self.queued.pop_front() {
            self.selected = step(&self.windows, self.selected.as_ref(), dir);
        }
        Ok(())
    }
    pub fn step(&mut self, generation: u64, dir: Direction) -> Result<(), Error> {
        self.move_selection(generation, dir)?;
        self.cycling = true;
        Ok(())
    }
    /// Panel-local arrows/Tab move the selection without starting IPC cycling.
    pub fn move_selection(&mut self, generation: u64, dir: Direction) -> Result<(), Error> {
        self.check_generation(generation)?;
        if !self.opened {
            return Err(Error::NotOpen);
        }
        if !self.loaded {
            if self.queued.len() >= SEQUENCE_WINDOW as usize {
                return Err(Error::QueueOverflow);
            }
            self.queued.push_back(dir);
        } else {
            self.selected = step(&self.windows, self.selected.as_ref(), dir);
        }
        Ok(())
    }
    pub fn hover(&mut self, generation: u64, key: &WindowKey) -> Result<(), Error> {
        self.check_generation(generation)?;
        if !self.opened {
            return Err(Error::NotOpen);
        }
        if !self.windows.contains(key) {
            return Err(Error::StaleIdentity);
        }
        if !self.cycling {
            self.selected = Some(key.clone());
        }
        Ok(())
    }
    /// Only returns a candidate. Backend activation_target and fresh OS validation remain required.
    pub fn activation_target(&self, generation: u64) -> Result<(WindowKey, u64), Error> {
        self.check_generation(generation)?;
        if !self.opened || !self.loaded {
            return Err(Error::NotOpen);
        }
        self.selected
            .clone()
            .map(|k| (k, self.revision))
            .ok_or(Error::StaleIdentity)
    }
    /// Legacy ordered `accept` only acts on a next/previous cycling session.
    /// Mouse/explicit activation uses activation_target instead.
    pub fn accept_cycle(&self, generation: u64) -> Result<(WindowKey, u64), Error> {
        self.check_generation(generation)?;
        if !self.cycling {
            return Err(Error::NotOpen);
        }
        self.activation_target(generation)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    Step(Direction),
    Accept,
    Cancel,
    Refresh,
}
struct Stream {
    next: u64,
    pending: BTreeMap<u64, Action>,
    last: u64,
}
pub struct OrderedStreams {
    streams: BTreeMap<String, Stream>,
    cap: usize,
    ttl: u64,
    time: u64,
}
impl OrderedStreams {
    pub fn new(cap: usize, ttl_ms: u64) -> Result<Self, Error> {
        if !(1..=MAX_STREAMS).contains(&cap) || !(1..=60_000).contains(&ttl_ms) {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            streams: BTreeMap::new(),
            cap,
            ttl: ttl_ms,
            time: 0,
        })
    }
    fn expire(&mut self, now: u64) -> Result<(), Error> {
        if now < self.time {
            return Err(Error::ClockWentBackwards);
        }
        self.time = now;
        self.streams.retain(|_, s| now - s.last < self.ttl);
        Ok(())
    }
    /// Adapter supplies a fresh authenticated session nonce. Expired streams require reopen;
    /// no automatic creation by an arbitrary old queued command.
    pub fn open(&mut self, id: &str, first_sequence: u64, now: u64) -> Result<(), Error> {
        if id.is_empty()
            || id.len() > 128
            || id.chars().any(char::is_control)
            || first_sequence == u64::MAX
        {
            return Err(Error::InvalidInput);
        }
        self.expire(now)?;
        if self.streams.contains_key(id) {
            return Err(Error::Duplicate);
        }
        if self.streams.len() >= self.cap {
            return Err(Error::LimitExceeded);
        }
        self.streams.insert(
            id.into(),
            Stream {
                next: first_sequence,
                pending: BTreeMap::new(),
                last: now,
            },
        );
        Ok(())
    }
    pub fn enqueue(
        &mut self,
        id: &str,
        sequence: u64,
        action: Action,
        now: u64,
    ) -> Result<Vec<Action>, Error> {
        self.expire(now)?;
        let stream = self.streams.get_mut(id).ok_or(Error::StreamExpired)?;
        if sequence == u64::MAX {
            return Err(Error::Exhausted);
        }
        if sequence < stream.next || stream.pending.contains_key(&sequence) {
            return Err(Error::Duplicate);
        }
        if sequence - stream.next > SEQUENCE_WINDOW {
            return Err(Error::SequenceGap);
        }
        stream.pending.insert(sequence, action);
        stream.last = now;
        let mut ready = Vec::new();
        while let Some(action) = stream.pending.remove(&stream.next) {
            ready.push(action);
            stream.next += 1;
        }
        Ok(ready)
    }
    pub fn close(&mut self, id: &str) {
        self.streams.remove(id);
    }
    pub fn len(&self) -> usize {
        self.streams.len()
    }
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }
    pub fn pending(&self) -> usize {
        self.streams.values().map(|s| s.pending.len()).sum()
    }
}
