use crate::{Error, MAX_CLIENTS};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Address(u64);
impl Address {
    /// Hyprland JSON uses 0x-prefixed addresses; socket2 event addresses omit it.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.is_empty() || s.len() > 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid);
        }
        let n = u64::from_str_radix(s, 16).map_err(|_| Error::Invalid)?;
        if n == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self(n))
    }
    pub fn canonical(self) -> String {
        format!("0x{:x}", self.0)
    }
}
/// Compositor-issued stable window ID, canonical lowercase hexadecimal. Zero is valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StableId(u64);
impl StableId {
    pub fn parse(raw: &str) -> Result<Self, Error> {
        if raw.is_empty() || raw.len() > 16 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid);
        }
        Ok(Self(
            u64::from_str_radix(raw, 16).map_err(|_| Error::Invalid)?,
        ))
    }
    pub fn canonical(self) -> String {
        format!("{:x}", self.0)
    }
}
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Identity {
    pub process_epoch: [u8; 16],
    pub instance: String,
    pub connection: u64,
    pub generation: u64,
    pub address: Address,
}
/// Caller supplies a new unpredictable process epoch on every daemon start.
/// One owner must serialize events/snapshots; IDs are observations, not dispatch permits.
pub struct Tracker {
    epoch: [u8; 16],
    instance: String,
    connection: u64,
    generation: u64,
    ready: bool,
    live: BTreeMap<Address, u64>,
}
impl Tracker {
    pub fn new(process_epoch: [u8; 16]) -> Result<Self, Error> {
        if process_epoch == [0; 16] {
            return Err(Error::Invalid);
        }
        Ok(Self {
            epoch: process_epoch,
            instance: String::new(),
            connection: 0,
            generation: 0,
            ready: false,
            live: BTreeMap::new(),
        })
    }
    /// Call on connection establishment, even when HIS is unchanged. Full resnapshot required.
    pub fn connect(&mut self, instance: &str) -> Result<u64, Error> {
        if instance.is_empty()
            || instance.len() > 128
            || !instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::Invalid);
        }
        self.invalidate();
        self.connection = self.connection.checked_add(1).ok_or(Error::Exhausted)?;
        self.instance = instance.into();
        Ok(self.connection)
    }
    pub fn invalidate(&mut self) {
        self.ready = false;
        self.live.clear();
    }
    /// Snapshot acceptance is explicit: native IPC provides no atomic snapshot/event barrier.
    /// Adapter must collect events around the query and restart if reconciliation is ambiguous.
    pub fn snapshot(&mut self, connection: u64, addresses: &[Address]) -> Result<(), Error> {
        if self.connection == 0 || self.connection != connection {
            return Err(Error::Stale);
        }
        if addresses.len() > MAX_CLIENTS {
            self.invalidate();
            return Err(Error::Limit);
        }
        let unique: BTreeSet<_> = addresses.iter().copied().collect();
        if unique.len() != addresses.len() {
            self.invalidate();
            return Err(Error::Invalid);
        }
        // Every accepted snapshot rotates all IDs: conservative against address reuse.
        let end = self
            .generation
            .checked_add(addresses.len() as u64)
            .ok_or_else(|| {
                self.invalidate();
                Error::Exhausted
            })?;
        let mut next = BTreeMap::new();
        for (index, address) in addresses.iter().enumerate() {
            next.insert(*address, self.generation + index as u64 + 1);
        }
        self.generation = end;
        self.live = next;
        self.ready = true;
        Ok(())
    }
    pub fn opened(&mut self, connection: u64, address: Address) -> Result<Identity, Error> {
        self.check(connection)?;
        if self.live.contains_key(&address) {
            self.invalidate();
            return Err(Error::Stale);
        }
        if self.live.len() >= MAX_CLIENTS {
            self.invalidate();
            return Err(Error::Limit);
        }
        self.generation = self.generation.checked_add(1).ok_or_else(|| {
            self.invalidate();
            Error::Exhausted
        })?;
        self.live.insert(address, self.generation);
        self.get(address).ok_or(Error::Stale)
    }
    pub fn closed(&mut self, connection: u64, address: Address) -> Result<(), Error> {
        self.check(connection)?;
        if self.live.remove(&address).is_none() {
            self.invalidate();
            return Err(Error::Stale);
        }
        Ok(())
    }
    fn check(&self, connection: u64) -> Result<(), Error> {
        if !self.ready || self.connection != connection {
            Err(Error::Stale)
        } else {
            Ok(())
        }
    }
    pub fn get(&self, address: Address) -> Option<Identity> {
        if !self.ready {
            return None;
        }
        self.live.get(&address).map(|generation| Identity {
            process_epoch: self.epoch,
            instance: self.instance.clone(),
            connection: self.connection,
            generation: *generation,
            address,
        })
    }
    pub fn is_current(&self, identity: &Identity) -> bool {
        self.get(identity.address).as_ref() == Some(identity)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reuse_reconnect_and_resnapshot_revoke() {
        let a = Address::parse("0x001a").unwrap();
        assert_eq!(a, Address::parse("1a").unwrap());
        let mut t = Tracker::new([1; 16]).unwrap();
        let c = t.connect("abc_123").unwrap();
        t.snapshot(c, &[a]).unwrap();
        let first = t.get(a).unwrap();
        t.closed(c, a).unwrap();
        let second = t.opened(c, a).unwrap();
        assert!(!t.is_current(&first));
        assert!(t.is_current(&second));
        t.snapshot(c, &[a]).unwrap();
        assert!(!t.is_current(&second));
        let third = t.get(a).unwrap();
        let c2 = t.connect("abc_123").unwrap();
        assert!(!t.is_current(&third));
        assert!(t.snapshot(c, &[a]).is_err());
        t.snapshot(c2, &[a]).unwrap();
        assert!(!t.is_current(&third));
    }
    #[test]
    fn malformed_and_duplicates_fail_closed() {
        for s in ["0", "0x", "0x1;exec", "-1", "0x10000000000000000"] {
            assert!(Address::parse(s).is_err());
        }
        let mut t = Tracker::new([1; 16]).unwrap();
        let c = t.connect("test").unwrap();
        let a = Address::parse("1").unwrap();
        t.snapshot(c, &[a]).unwrap();
        assert!(t.opened(c, a).is_err());
        assert!(t.get(a).is_none());
        t.snapshot(c, &[a]).unwrap();
        assert!(t.snapshot(c, &[a, a]).is_err());
        assert!(t.get(a).is_none());
    }
    #[test]
    fn counters_never_wrap() {
        let mut t = Tracker::new([1; 16]).unwrap();
        let c = t.connect("test").unwrap();
        t.generation = u64::MAX;
        assert!(matches!(
            t.snapshot(c, &[Address(1)]),
            Err(Error::Exhausted)
        ));
        assert!(!t.ready);
        t.connection = u64::MAX;
        assert!(matches!(t.connect("test"), Err(Error::Exhausted)));
        assert!(!t.ready);
    }
}
