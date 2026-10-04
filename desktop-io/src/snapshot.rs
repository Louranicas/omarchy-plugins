use crate::{Address, Endpoint, Error, Event, Events, Identity, MAX_CLIENTS, Query, Tracker};
use std::time::{Duration, Instant};
/// Opaque callback identity: other processes/connections/requests cannot complete it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotTicket {
    epoch: [u8; 16],
    connection: u64,
    request: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOutcome {
    Applied,
    ResnapshotRequired,
}
/// One serialized owner coordinates request callbacks and the event stream.
/// Native IPC cannot atomically fence a future dispatch; this only reconciles observations.
pub struct Snapshots {
    tracker: Tracker,
    epoch: [u8; 16],
    instance: String,
    stream: u64,
    connection: u64,
    request: u64,
    pending: Option<SnapshotTicket>,
    synchronized: bool,
}
impl Snapshots {
    pub fn new(epoch: [u8; 16]) -> Result<Self, Error> {
        Ok(Self {
            tracker: Tracker::new(epoch)?,
            epoch,
            instance: String::new(),
            stream: 0,
            connection: 0,
            request: 0,
            pending: None,
            synchronized: false,
        })
    }
    fn attach(&mut self, instance: &str, stream: u64) -> Result<(), Error> {
        if stream == 0 || stream < self.stream {
            return Err(Error::Stale);
        }
        if self.stream != stream || self.instance != instance {
            self.invalidate();
            self.connection = self.tracker.connect(instance)?;
            self.stream = stream;
            self.instance = instance.into();
        }
        Ok(())
    }
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.synchronized = false;
        self.tracker.invalidate();
    }
    pub fn identity(&self, address: Address) -> Option<Identity> {
        self.tracker.get(address)
    }
    pub fn is_current(&self, identity: &Identity) -> bool {
        self.tracker.is_current(identity)
    }
    pub fn begin(&mut self) -> Result<SnapshotTicket, Error> {
        self.invalidate();
        if self.connection == 0 {
            return Err(Error::Stale);
        }
        self.request = self.request.checked_add(1).ok_or(Error::Exhausted)?;
        let ticket = SnapshotTicket {
            epoch: self.epoch,
            connection: self.connection,
            request: self.request,
        };
        self.pending = Some(ticket);
        Ok(ticket)
    }
    pub fn complete(
        &mut self,
        ticket: SnapshotTicket,
        clients: &serde_json::Value,
    ) -> Result<(), Error> {
        if self.pending != Some(ticket) {
            return Err(Error::Stale);
        }
        self.pending = None;
        let rows = clients.as_array().ok_or(Error::Invalid)?;
        if rows.len() > MAX_CLIENTS {
            return Err(Error::Limit);
        }
        let addresses = rows
            .iter()
            .map(|row| {
                row.get("address")
                    .and_then(|v| v.as_str())
                    .ok_or(Error::Invalid)
                    .and_then(Address::parse)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.tracker.snapshot(self.connection, &addresses)?;
        self.synchronized = true;
        Ok(())
    }
    fn observe(&mut self, event: &Event) -> Result<EventOutcome, Error> {
        // Every event seen during a request makes that snapshot ambiguous, even
        // a non-lifecycle event: deliberately restart instead of guessing order.
        if self.pending.take().is_some() {
            self.tracker.invalidate();
            return Ok(EventOutcome::ResnapshotRequired);
        }
        if !self.synchronized {
            return Ok(EventOutcome::ResnapshotRequired);
        }
        let result = match event.name.as_str() {
            "openwindow" => {
                let fields: Vec<_> = event.payload.splitn(4, ',').collect();
                if fields.len() != 4 {
                    Err(Error::Invalid)
                } else {
                    Address::parse(fields[0])
                        .and_then(|a| self.tracker.opened(self.connection, a).map(|_| ()))
                }
            }
            "closewindow" => {
                Address::parse(&event.payload).and_then(|a| self.tracker.closed(self.connection, a))
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            self.invalidate();
            return Err(e);
        }
        Ok(EventOutcome::Applied)
    }
    /// Poll at most one complete event; None means no complete frame available.
    /// Fatal stream errors revoke identities before being returned.
    pub fn poll(&mut self, events: &mut Events) -> Result<Option<(Event, EventOutcome)>, Error> {
        self.attach(events.instance(), events.stream_id())?;
        match events.poll_event() {
            Ok(Some(event)) => {
                let outcome = self.observe(&event)?;
                Ok(Some((event, outcome)))
            }
            Ok(None) => {
                if events.has_partial() {
                    self.invalidate();
                }
                Ok(None)
            }
            Err(error) => {
                self.invalidate();
                Err(error)
            }
        }
    }
    fn drain(&mut self, events: &mut Events, deadline: Instant) -> Result<(), Error> {
        for _ in 0..256 {
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            if self.poll(events)?.is_none() {
                return if events.has_partial() {
                    Err(Error::Stale)
                } else {
                    Ok(())
                };
            }
        }
        Err(Error::Limit)
    }
    /// Drain queued events, issue clients query, drain again, then commit only if
    /// no event was observed during the query. 256-event drain cap; no hidden retry.
    /// The caller decides bounded backoff when contention returns Stale.
    pub fn collect(
        &mut self,
        endpoint: &Endpoint,
        events: &mut Events,
        budget: Duration,
    ) -> Result<(), Error> {
        self.collect_clients(endpoint, events, budget).map(|_| ())
    }
    /// Same checked collection, retaining the JSON clients for application metadata decoding.
    pub fn collect_clients(
        &mut self,
        endpoint: &Endpoint,
        events: &mut Events,
        budget: Duration,
    ) -> Result<serde_json::Value, Error> {
        if budget.is_zero() || budget > Duration::from_secs(5) {
            return Err(Error::Invalid);
        }
        if !endpoint.owns(events) {
            self.invalidate();
            return Err(Error::Unauthenticated);
        }
        let deadline = Instant::now().checked_add(budget).ok_or(Error::Invalid)?;
        self.attach(events.instance(), events.stream_id())?;
        let result = (|| {
            // Preexisting events need not apply to an initially empty tracker.
            // Drain them under a pending ticket, ensuring no old observations survive.
            self.begin()?;
            self.drain(events, deadline)?;
            let ticket = self.begin()?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Deadline)?;
            let clients = endpoint.query(Query::Clients, remaining)?;
            self.drain(events, deadline)?;
            self.complete(ticket, &clients)?;
            Ok(clients)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> Snapshots {
        let mut s = Snapshots::new([1; 16]).unwrap();
        s.attach("test", 1).unwrap();
        s
    }
    #[test]
    fn late_duplicate_and_other_process_tickets_rejected() {
        let mut s = state();
        let a = s.begin().unwrap();
        let b = s.begin().unwrap();
        let rows = serde_json::json!([{"address":"0x1"}]);
        assert!(s.complete(a, &rows).is_err());
        s.complete(b, &rows).unwrap();
        assert!(s.complete(b, &rows).is_err());
        let other = Snapshots::new([2; 16]).unwrap();
        assert!(!other.is_current(&s.identity(Address::parse("1").unwrap()).unwrap()));
    }
    #[test]
    fn intervening_event_and_reconnect_revoke_callback() {
        let mut s = state();
        let t = s.begin().unwrap();
        assert_eq!(
            s.observe(&Event::parse(b"workspace>>2").unwrap()).unwrap(),
            EventOutcome::ResnapshotRequired
        );
        assert!(s.complete(t, &serde_json::json!([])).is_err());
        let t = s.begin().unwrap();
        s.attach("test", 2).unwrap();
        assert!(s.complete(t, &serde_json::json!([])).is_err());
    }
    #[test]
    fn lifecycle_address_reuse_is_fenced() {
        let mut s = state();
        let t = s.begin().unwrap();
        s.complete(t, &serde_json::json!([{"address":"1"}]))
            .unwrap();
        let old = s.identity(Address::parse("1").unwrap()).unwrap();
        s.observe(&Event::parse(b"closewindow>>1").unwrap())
            .unwrap();
        s.observe(&Event::parse(b"openwindow>>1,2,class,title,with,commas").unwrap())
            .unwrap();
        assert!(!s.is_current(&old));
    }
    #[test]
    fn invalid_snapshot_and_counter_exhaustion_fail_closed() {
        let mut s = state();
        for rows in [
            serde_json::json!({}),
            serde_json::json!([{}]),
            serde_json::json!([{"address":"1"},{"address":"0x1"}]),
        ] {
            let t = s.begin().unwrap();
            assert!(s.complete(t, &rows).is_err());
            assert!(s.identity(Address::parse("1").unwrap()).is_none());
        }
        s.request = u64::MAX;
        assert!(matches!(s.begin(), Err(Error::Exhausted)));
    }
}
