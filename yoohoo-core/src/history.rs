use crate::{Error, config::Config};
use serde::Serialize;
use std::collections::VecDeque;

/// This is the entire retained schema. There is deliberately no free-form text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryEvent {
    Attention,
    Focused,
    Closed,
    Cleared,
    ServiceStopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Record {
    pub schema_version: u32,
    pub opaque_id: u64,
    pub at_ms: i64,
    pub event: HistoryEvent,
    pub count: u64,
    pub source_bits: u8,
}
struct Stored {
    inserted: u64,
    json: Vec<u8>,
}
#[derive(Default)]
pub struct History {
    entries: VecDeque<Stored>,
    bytes: usize,
    last_time: Option<u64>,
}
impl History {
    pub fn record(&mut self, record: Record, now: u64, config: &Config) -> Result<(), Error> {
        config.validate()?;
        self.check_time(now)?;
        if !config.history_enabled {
            return Ok(());
        }
        let mut json = serde_json::to_vec(&record).map_err(|_| Error::InvalidInput)?;
        json.push(b'\n');
        if json.len() > config.history_bytes {
            return Err(Error::LimitExceeded);
        }
        self.expire(now, config)?;
        while self.entries.len() >= config.history_entries
            || self.bytes + json.len() > config.history_bytes
        {
            self.remove_oldest();
        }
        self.bytes += json.len();
        self.entries.push_back(Stored {
            inserted: now,
            json,
        });
        Ok(())
    }
    fn check_time(&mut self, now: u64) -> Result<(), Error> {
        if self.last_time.is_some_and(|last| now < last) {
            return Err(Error::ClockWentBackwards);
        }
        self.last_time = Some(now);
        Ok(())
    }
    pub fn expire(&mut self, now: u64, config: &Config) -> Result<(), Error> {
        config.validate()?;
        self.check_time(now)?;
        while self
            .entries
            .front()
            .is_some_and(|v| now - v.inserted >= config.history_age_ms)
            || self.entries.len() > config.history_entries
            || self.bytes > config.history_bytes
        {
            self.remove_oldest();
        }
        Ok(())
    }
    fn remove_oldest(&mut self) {
        if let Some(old) = self.entries.pop_front() {
            self.bytes -= old.json.len();
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn lines(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.iter().map(|e| e.json.as_slice())
    }
    /// Explicit purge; disabling capture does not silently delete prior history.
    pub fn purge(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}
