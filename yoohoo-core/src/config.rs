use crate::Error;
use std::collections::BTreeMap;

pub const MAX_WINDOWS: usize = 4096;
pub const MAX_METADATA_BYTES: usize = 4096;
pub const MAX_STREAMS: usize = 64;
pub const SEQUENCE_WINDOW: u64 = 256;
pub const HISTORY_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const HISTORY_BYTES: usize = 10 * 1024 * 1024;
pub const HISTORY_ENTRIES: usize = 10_000;

/// Typed validated settings only. Parsing/migrating third-party TOML belongs to an adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub max_windows: usize,
    pub queue_events: usize,
    pub streams: usize,
    pub stream_ttl_ms: u64,
    pub sound_enabled: bool,
    pub volume: f64,
    pub cooldown_ms: u64,
    pub history_enabled: bool,
    pub history_age_ms: u64,
    pub history_bytes: usize,
    pub history_entries: usize,
    pub reduced_motion: bool,
    pub extensions: BTreeMap<String, String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_windows: MAX_WINDOWS,
            queue_events: 1024,
            streams: MAX_STREAMS,
            stream_ttl_ms: 60_000,
            sound_enabled: true,
            volume: 0.35,
            cooldown_ms: 1500,
            history_enabled: false,
            history_age_ms: HISTORY_DAYS_MS,
            history_bytes: HISTORY_BYTES,
            history_entries: HISTORY_ENTRIES,
            reduced_motion: false,
            extensions: BTreeMap::new(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=MAX_WINDOWS).contains(&self.max_windows)
            || !(1..=1024).contains(&self.queue_events)
            || !(1..=MAX_STREAMS).contains(&self.streams)
            || !(1..=60_000).contains(&self.stream_ttl_ms)
            || !self.volume.is_finite()
            || !(0.0..=1.0).contains(&self.volume)
            || self.cooldown_ms > 60_000
            || !(1..=HISTORY_DAYS_MS).contains(&self.history_age_ms)
            || !(1..=HISTORY_BYTES).contains(&self.history_bytes)
            || !(1..=HISTORY_ENTRIES).contains(&self.history_entries)
            || self.extensions.len() > 32
            || self.extensions.iter().any(|(k, v)| {
                k.is_empty() || k.len() > 64 || v.len() > 1024 || k.chars().any(char::is_control)
            })
        {
            return Err(Error::InvalidConfig);
        }
        Ok(())
    }
}

/// One process owner must hold this gate. Adapters enforce the corresponding child cap.
#[derive(Debug, Default)]
pub struct SoundGate {
    last_started: Option<u64>,
    playing: Option<u64>,
    next: u64,
}
impl SoundGate {
    pub fn request(&mut self, now: u64, config: &Config) -> Option<u64> {
        if !config.sound_enabled || self.playing.is_some() {
            return None;
        }
        if let Some(last) = self.last_started
            && (now < last || now - last < config.cooldown_ms)
        {
            return None;
        }
        let token = self.next.checked_add(1)?;
        self.next = token;
        self.last_started = Some(now);
        self.playing = Some(token);
        Some(token)
    }
    pub fn finished(&mut self, token: u64) -> Result<(), Error> {
        if self.playing != Some(token) {
            return Err(Error::StaleIdentity);
        }
        self.playing = None;
        Ok(())
    }
    pub fn playing(&self) -> Option<u64> {
        self.playing
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pulse {
    Inhale,
    Exhale,
    Rest,
    Steady,
}
pub fn pulse_at(elapsed_ms: u64, reduced_motion: bool) -> Pulse {
    if reduced_motion {
        return Pulse::Steady;
    }
    match elapsed_ms % 3600 {
        0..=1599 => Pulse::Inhale,
        1600..=3199 => Pulse::Exhale,
        _ => Pulse::Rest,
    }
}
