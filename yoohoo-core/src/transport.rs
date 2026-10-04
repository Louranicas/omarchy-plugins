use crate::{Error, identity::Address};
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeEvent {
    Urgent(Address),
    Focused(Option<Address>),
    Closed(Address),
    Refresh(Address),
    Ignored,
}
/// Frame limit is checked before interpretation; title/body fragments are discarded.
/// Streaming IO must independently cap bytes before allocating the line.
pub fn parse_native(line: &str) -> Result<NativeEvent, Error> {
    if line.len() > 8192 {
        return Err(Error::LimitExceeded);
    }
    let line = line.strip_suffix('\n').unwrap_or(line);
    let (kind, data) = line.split_once(">>").ok_or(Error::InvalidInput)?;
    let address = |raw: &str| -> Result<Address, Error> {
        if raw.starts_with("0x") {
            Address::parse(raw)
        } else {
            Address::parse(&format!("0x{raw}"))
        }
    };
    match kind {
        "urgent" => Ok(NativeEvent::Urgent(address(data)?)),
        "activewindowv2" => Ok(NativeEvent::Focused(if data.is_empty() {
            None
        } else {
            Some(address(data)?)
        })),
        "closewindow" => Ok(NativeEvent::Closed(address(data)?)),
        "windowtitle" | "windowtitlev2" | "movewindow" | "movewindowv2" => Ok(
            NativeEvent::Refresh(address(data.split(',').next().unwrap_or_default())?),
        ),
        _ => Ok(NativeEvent::Ignored),
    }
}
/// Count-bound queue for already byte-bounded decoded events. Overflow requires full resync;
/// it cannot quietly lose focus/close events and pretend to preserve a complete event stream.
pub struct EventQueue<T> {
    queue: VecDeque<T>,
    cap: usize,
    needs_resync: bool,
}
impl<T> EventQueue<T> {
    pub fn new(cap: usize) -> Result<Self, Error> {
        if !(1..=1024).contains(&cap) {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            queue: VecDeque::new(),
            cap,
            needs_resync: false,
        })
    }
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        if self.needs_resync {
            return Err(Error::SourceUnavailable);
        }
        if self.queue.len() == self.cap {
            self.queue.clear();
            self.needs_resync = true;
            return Err(Error::QueueOverflow);
        }
        self.queue.push_back(value);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }
    pub fn needs_resync(&self) -> bool {
        self.needs_resync
    }
    pub fn resynchronized(&mut self) {
        self.queue.clear();
        self.needs_resync = false;
    }
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
/// One latest full-state value, not a lossy incremental-event queue.
#[derive(Default)]
pub struct LatestSnapshot<T>(Option<T>);
impl<T> LatestSnapshot<T> {
    pub fn offer(&mut self, value: T) {
        self.0 = Some(value);
    }
    pub fn take(&mut self) -> Option<T> {
        self.0.take()
    }
}
#[derive(Default, Debug)]
pub struct Reconnect {
    attempt: u8,
}
impl Reconnect {
    /// Supplied jitter avoids hidden randomness in tests. Host selects actual endpoint by
    /// explicit compositor instance; this policy never falls back to a different instance.
    pub fn delay_ms(&mut self, jitter: u16) -> u64 {
        let base = (250u64 << self.attempt.min(5)).min(10_000);
        self.attempt = self.attempt.saturating_add(1);
        (base + u64::from(jitter) % 251).min(10_000)
    }
    pub fn connected(&mut self) {
        self.attempt = 0;
    }
}
