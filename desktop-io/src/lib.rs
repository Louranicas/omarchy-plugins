//! Bounded Linux Hyprland transport. No compositor mutation API.
#![cfg(target_os = "linux")]
mod identity;
mod snapshot;
pub use snapshot::{EventOutcome, SnapshotTicket, Snapshots};
mod json;
mod peer;
pub use peer::ProcessIdentity;
mod transport;
pub use identity::{Address, Identity, StableId, Tracker};
use std::{fmt, io};
pub use transport::{DispatchDialect, Endpoint, Event, Events, ModalSubmap, Query};
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Invalid,
    Limit,
    Deadline,
    Unauthenticated,
    Disconnected,
    Stale,
    Exhausted,
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "desktop transport: {self:?}")
    }
}
impl std::error::Error for Error {}
pub const MAX_CLIENTS: usize = 4096;
pub const MAX_RESPONSE: usize = 4 * 1024 * 1024;
pub const MAX_EVENT: usize = 16 * 1024;
