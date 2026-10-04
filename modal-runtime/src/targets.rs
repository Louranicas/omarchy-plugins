//! Arbiter-owned current modal targets. Tokens are observations, never standalone authority.
use modal_contract::Fence;
use std::collections::BTreeSet;

pub const MAX_TARGETS: usize = 4096;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeTarget {
    instance: String,
    stable_id: String,
}
impl NativeTarget {
    pub fn new(instance: &str, stable_id: &str) -> Result<Self, Error> {
        if instance.is_empty()
            || instance.len() > 128
            || !instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || stable_id.is_empty()
            || stable_id.len() > 16
            || !stable_id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Invalid);
        }
        let id = u64::from_str_radix(stable_id, 16).map_err(|_| Error::Invalid)?;
        Ok(Self {
            instance: instance.into(),
            stable_id: format!("{id:x}"),
        })
    }
    pub fn instance(&self) -> &str {
        &self.instance
    }
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }
    /// Exact native selector; never accepts raw dispatch fragments or an address fallback.
    pub fn selector(&self) -> String {
        format!("stableid:{}", self.stable_id)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token(String);
impl Token {
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.len() != 48
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid);
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTicket {
    epoch: [u8; 16],
    generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusIntent {
    pub token: Token,
    pub displayed_revision: u64,
    pub producer_session: [u8; 16],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub token: Token,
    pub target: NativeTarget,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub revision: u64,
    pub producer_session: [u8; 16],
    pub rows: Vec<Row>,
    pub next: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Stale,
    Unavailable,
    Limit,
    Exhausted,
}
struct View {
    fence: Fence,
    session: [u8; 16],
    revision: u64,
    rows: Vec<Row>,
}
pub struct Registry {
    epoch: [u8; 16],
    source: u64,
    sequence: u64,
    revision: u64,
    serial: u64,
    ready: bool,
    broken: bool,
    current: BTreeSet<NativeTarget>,
    view: Option<View>,
}
impl Registry {
    pub fn new(epoch: [u8; 16]) -> Self {
        Self {
            epoch,
            source: 0,
            sequence: 0,
            revision: 0,
            serial: 0,
            ready: false,
            broken: false,
            current: BTreeSet::new(),
            view: None,
        }
    }
    fn bump(&mut self) -> Result<(), Error> {
        self.view = None;
        self.revision = self.revision.checked_add(1).ok_or_else(|| {
            self.ready = false;
            Error::Exhausted
        })?;
        Ok(())
    }
    /// Only the trusted arbiter observer calls this upon a real source connection.
    pub fn begin_source(&mut self) -> Result<SourceTicket, Error> {
        self.ready = false;
        self.current.clear();
        self.bump()?;
        self.source = self.source.checked_add(1).ok_or(Error::Exhausted)?;
        self.sequence = 0;
        self.broken = false;
        Ok(SourceTicket {
            epoch: self.epoch,
            generation: self.source,
        })
    }
    fn accept(&mut self, ticket: SourceTicket, sequence: u64) -> Result<(), Error> {
        if self.broken
            || ticket.epoch != self.epoch
            || ticket.generation != self.source
            || self.source == 0
        {
            return Err(Error::Stale);
        }
        if self.sequence.checked_add(1) != Some(sequence) {
            self.broken = true;
            self.ready = false;
            self.current.clear();
            self.bump()?;
            return Err(Error::Stale);
        }
        self.sequence = sequence;
        Ok(())
    }
    pub fn snapshot(
        &mut self,
        ticket: SourceTicket,
        sequence: u64,
        targets: Vec<NativeTarget>,
    ) -> Result<(), Error> {
        self.accept(ticket, sequence)?;
        self.ready = false;
        self.view = None;
        if targets.len() > MAX_TARGETS {
            self.current.clear();
            self.bump()?;
            return Err(Error::Limit);
        }
        let current: BTreeSet<_> = targets.iter().cloned().collect();
        if current.len() != targets.len()
            || current.iter().any(|target| {
                current
                    .first()
                    .is_some_and(|first| first.instance() != target.instance())
            })
        {
            self.current.clear();
            self.bump()?;
            return Err(Error::Invalid);
        }
        self.bump()?;
        self.current = current;
        self.ready = true;
        Ok(())
    }
    pub fn closed(
        &mut self,
        ticket: SourceTicket,
        sequence: u64,
        target: &NativeTarget,
    ) -> Result<(), Error> {
        self.accept(ticket, sequence)?;
        if !self.ready || !self.current.remove(target) {
            self.ready = false;
            self.current.clear();
            self.bump()?;
            return Err(Error::Stale);
        }
        self.bump()
    }
    pub fn lost(&mut self, ticket: SourceTicket, sequence: u64) -> Result<(), Error> {
        self.accept(ticket, sequence)?;
        self.ready = false;
        self.current.clear();
        self.bump()
    }
    pub fn revoke_view(&mut self) {
        self.view = None;
    }
    pub fn page(&mut self, fence: Fence, session: [u8; 16], cursor: usize) -> Result<Page, Error> {
        if !self.ready {
            return Err(Error::Unavailable);
        }
        if self
            .view
            .as_ref()
            .is_none_or(|v| v.fence != fence || v.session != session || v.revision != self.revision)
        {
            if cursor != 0 {
                return Err(Error::Stale);
            }
            let mut rows = Vec::with_capacity(self.current.len());
            for target in &self.current {
                self.serial = self.serial.checked_add(1).ok_or_else(|| {
                    self.ready = false;
                    self.view = None;
                    Error::Exhausted
                })?;
                let prefix: String = self
                    .epoch
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                rows.push(Row {
                    token: Token(format!("{prefix}{:016x}", self.serial)),
                    target: target.clone(),
                });
            }
            self.view = Some(View {
                fence,
                session,
                revision: self.revision,
                rows,
            });
        }
        let view = self.view.as_ref().ok_or(Error::Unavailable)?;
        if cursor > view.rows.len() {
            return Err(Error::Invalid);
        }
        let end = cursor.saturating_add(8).min(view.rows.len());
        Ok(Page {
            revision: self.revision,
            producer_session: session,
            rows: view.rows[cursor..end].to_vec(),
            next: (end < view.rows.len()).then_some(end),
        })
    }
    pub fn resolve(&self, fence: Fence, intent: &FocusIntent) -> Result<NativeTarget, Error> {
        if !self.ready {
            return Err(Error::Unavailable);
        }
        let view = self.view.as_ref().ok_or(Error::Stale)?;
        if view.fence != fence
            || view.session != intent.producer_session
            || view.revision != intent.displayed_revision
            || self.revision != intent.displayed_revision
        {
            return Err(Error::Stale);
        }
        view.rows
            .iter()
            .find(|row| row.token == intent.token)
            .filter(|row| self.current.contains(&row.target))
            .map(|row| row.target.clone())
            .ok_or(Error::Stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modal_contract::{AuthorityEpoch, ClientId, Owner};
    use std::num::NonZeroU64;
    fn fence() -> Fence {
        Fence {
            authority_epoch: AuthorityEpoch([7; 16]),
            owner: Owner {
                client: ClientId([1; 16]),
                client_epoch: NonZeroU64::new(1).unwrap(),
            },
            generation: NonZeroU64::new(1).unwrap(),
        }
    }
    fn setup() -> (Registry, SourceTicket, NativeTarget) {
        let mut r = Registry::new([7; 16]);
        let s = r.begin_source().unwrap();
        let target = NativeTarget::new("instance", "f").unwrap();
        r.snapshot(s, 1, vec![target.clone()]).unwrap();
        (r, s, target)
    }
    fn intent(r: &mut Registry) -> FocusIntent {
        let p = r.page(fence(), [8; 16], 0).unwrap();
        FocusIntent {
            token: p.rows[0].token.clone(),
            displayed_revision: p.revision,
            producer_session: p.producer_session,
        }
    }
    #[test]
    fn targets_are_bound_to_full_fence_session_and_displayed_revision() {
        let (mut r, _, target) = setup();
        let good = intent(&mut r);
        assert_eq!(r.resolve(fence(), &good).unwrap(), target);
        let mut other = fence();
        other.owner.client_epoch = NonZeroU64::new(2).unwrap();
        assert_eq!(r.resolve(other, &good), Err(Error::Stale));
        let mut bad = good.clone();
        bad.producer_session = [9; 16];
        assert_eq!(r.resolve(fence(), &bad), Err(Error::Stale));
        bad = good.clone();
        bad.displayed_revision += 1;
        assert_eq!(r.resolve(fence(), &bad), Err(Error::Stale));
        r.revoke_view();
        assert_eq!(r.resolve(fence(), &good), Err(Error::Stale));
    }
    #[test]
    fn close_reuse_loss_and_restart_never_reanimate_tokens() {
        let (mut r, s, t) = setup();
        let old = intent(&mut r);
        r.closed(s, 2, &t).unwrap();
        assert!(r.resolve(fence(), &old).is_err());
        r.snapshot(s, 3, vec![t]).unwrap();
        let new = intent(&mut r);
        assert_ne!(old.token, new.token);
        assert!(r.resolve(fence(), &old).is_err());
        r.lost(s, 4).unwrap();
        assert_eq!(r.resolve(fence(), &new), Err(Error::Unavailable));
        let mut replacement = Registry::new([6; 16]);
        assert_eq!(replacement.snapshot(s, 5, vec![]), Err(Error::Stale));
    }
    #[test]
    fn late_old_source_cannot_replace_or_invalidate_current_source() {
        let (mut r, old, t) = setup();
        let new = r.begin_source().unwrap();
        r.snapshot(new, 1, vec![t]).unwrap();
        let good = intent(&mut r);
        assert_eq!(r.lost(old, 2), Err(Error::Stale));
        assert!(r.resolve(fence(), &good).is_ok());
        assert_eq!(r.snapshot(old, 3, vec![]), Err(Error::Stale));
        assert!(r.resolve(fence(), &good).is_ok());
        assert_eq!(r.snapshot(new, 3, vec![]), Err(Error::Stale));
        assert!(r.resolve(fence(), &good).is_err());
        assert_eq!(r.snapshot(new, 2, vec![]), Err(Error::Stale));
    }
    #[test]
    fn malformed_mixed_duplicate_and_oversize_snapshots_fail_closed() {
        for targets in [
            vec![
                NativeTarget::new("a", "0").unwrap(),
                NativeTarget::new("b", "0").unwrap(),
            ],
            vec![NativeTarget::new("a", "0").unwrap(); 2],
            vec![NativeTarget::new("a", "0").unwrap(); MAX_TARGETS + 1],
        ] {
            let (mut r, s, _) = setup();
            let old = intent(&mut r);
            assert!(r.snapshot(s, 2, targets).is_err());
            assert!(r.resolve(fence(), &old).is_err());
        }
        for id in ["", "1,exec", "0x1", "fffffffffffffffff", "1\n"] {
            assert!(NativeTarget::new("a", id).is_err());
        }
        assert_eq!(
            NativeTarget::new("a", "000AF").unwrap().selector(),
            "stableid:af"
        );
    }
    #[test]
    fn pages_are_bounded_and_cursors_cannot_switch_view() {
        let (mut r, s, _) = setup();
        r.snapshot(
            s,
            2,
            (0..30)
                .map(|i| NativeTarget::new("a", &format!("{i:x}")).unwrap())
                .collect(),
        )
        .unwrap();
        let p = r.page(fence(), [8; 16], 0).unwrap();
        assert_eq!(p.rows.len(), 8);
        assert_eq!(p.next, Some(8));
        assert_eq!(r.page(fence(), [9; 16], 8), Err(Error::Stale));
        let q = r.page(fence(), [8; 16], 8).unwrap();
        assert_eq!(p.revision, q.revision);
        assert_ne!(p.rows[0].token, q.rows[0].token);
    }
}
