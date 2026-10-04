//! Lease-scoped orchestration. This module emits intents, never compositor calls.
//! Only the trusted host may supply readiness or completed-operation evidence.
pub mod action_plan;
pub mod native;
pub mod policy;
pub mod presenter;
pub mod service;
use modal_contract::Fence;
use std::collections::BTreeMap;
use vimarchy_core::{allocation, gestures, snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub presenter: [u8; 16],
    pub fence: Fence,
    pub source_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use modal_contract::{AuthorityEpoch, ClientId, Owner};
    use std::num::NonZeroU64;
    fn context() -> Context {
        Context {
            presenter: [3; 16],
            source_revision: 1,
            fence: Fence {
                authority_epoch: AuthorityEpoch([1; 16]),
                owner: Owner {
                    client: ClientId([2; 16]),
                    client_epoch: NonZeroU64::new(1).unwrap(),
                },
                generation: NonZeroU64::new(1).unwrap(),
            },
        }
    }
    fn fixture() -> Session {
        let monitors = [snapshot::Monitor {
            name: "fixture".into(),
            x: 0.,
            y: 0.,
            width: 1280.,
            height: 720.,
            scale: 1.,
            active_workspace: snapshot::Workspace { id: 1 },
            special_workspace: snapshot::Workspace { id: 0 },
        }];
        let clients = [snapshot::Client {
            address: "0x1".into(),
            stable_id: "1".into(),
            mapped: true,
            hidden: false,
            pinned: false,
            workspace: snapshot::Workspace { id: 1 },
            at: [10., 10.],
            size: [400., 300.],
            grouped: vec![],
            focus_history_id: 0,
        }];
        let (session, batch) = Session::open(
            context(),
            1000,
            0,
            &monitors,
            &clients,
            &BTreeMap::new(),
            250,
        )
        .unwrap();
        assert_eq!(batch.effects, [gestures::Effect::Show]);
        session
    }
    #[test]
    fn host_proof_is_separate_from_ui_map_and_completion() {
        let mut s = fixture();
        assert_eq!(
            s.input(context(), gestures::Event::Mapped, 1).unwrap_err(),
            Error::HostOnlyEvent
        );
        assert_eq!(
            s.input(
                context(),
                gestures::Event::HoldCompleted {
                    operation_id: 1,
                    completed: true
                },
                1
            )
            .unwrap_err(),
            Error::HostOnlyEvent
        );
        let ready = s.host_ready(context(), 1).unwrap();
        assert_eq!(
            ready.effects,
            [gestures::Effect::EnterSelection { two_letters: false }]
        );
    }
    #[test]
    fn invalidation_or_expiry_between_press_and_release_cannot_focus() {
        for invalidate in [false, true] {
            let mut session = fixture();
            session.host_ready(context(), 1).unwrap();
            session
                .input(
                    context(),
                    gestures::Event::Down {
                        hint: "a".into(),
                        press_id: 1,
                        alt: false,
                        repeat: false,
                    },
                    2,
                )
                .unwrap();
            if invalidate {
                session.invalidate();
            }
            let result = session.input(
                context(),
                gestures::Event::Up { press_id: 1 },
                if invalidate { 3 } else { 1000 },
            );
            assert!(result.is_err());
            assert!(!session.is_valid());
        }
    }
    #[test]
    fn expiry_inclusive_cannot_be_revived_by_renewal() {
        let mut s = fixture();
        assert_eq!(
            s.input(context(), gestures::Event::Tick, 1000).unwrap_err(),
            Error::Expired
        );
        assert!(!s.is_valid());
        assert_eq!(s.renew(context(), 2000, 1000), Err(Error::Stale));
    }
    #[test]
    fn replacement_nonce_revision_and_full_fence_are_required() {
        let mut s = fixture();
        let mut alternatives = [context(); 4];
        alternatives[0].presenter = [9; 16];
        alternatives[1].source_revision = 2;
        alternatives[2].fence.authority_epoch = AuthorityEpoch([9; 16]);
        alternatives[3].fence.owner.client_epoch = NonZeroU64::new(2).unwrap();
        for c in alternatives {
            assert_eq!(s.host_ready(c, 1).unwrap_err(), Error::Stale);
        }
        assert!(s.host_ready(context(), 1).is_ok());
    }
    #[test]
    fn invalidation_revokes_already_computed_batch() {
        let mut s = fixture();
        let batch = s.host_ready(context(), 1).unwrap();
        s.invalidate();
        assert_eq!(s.consume_batch(batch, 2), Err(Error::Stale));
    }
    #[test]
    fn monotonic_time_lease_bounds_and_deadline_overflow() {
        let mut s = fixture();
        s.host_ready(context(), 10).unwrap();
        assert_eq!(
            s.input(context(), gestures::Event::Tick, 9).unwrap_err(),
            Error::ClockRegression
        );
        assert_eq!(s.renew(context(), u64::MAX, 10), Err(Error::InvalidLease));
        assert_eq!(s.renew(context(), 999, 10), Err(Error::InvalidLease));
        assert!(s.renew(context(), 1500, 10).is_ok());
        assert!(s.input(context(), gestures::Event::Tick, 1000).is_ok());
    }
    #[test]
    fn queued_focus_is_revoked_by_cancel_and_new_transitions() {
        let mut s = fixture();
        s.host_ready(context(), 1).unwrap();
        s.input(
            context(),
            gestures::Event::Down {
                hint: "a".into(),
                press_id: 1,
                alt: false,
                repeat: false,
            },
            2,
        )
        .unwrap();
        let focus = s
            .input(context(), gestures::Event::Up { press_id: 1 }, 3)
            .unwrap();
        assert!(
            focus
                .effects()
                .iter()
                .any(|e| matches!(e, gestures::Effect::Focus(_)))
        );
        let cancel = s.input(context(), gestures::Event::Cancel, 4).unwrap();
        assert_eq!(s.consume_batch(focus, 4), Err(Error::Stale));
        assert!(s.consume_batch(cancel, 4).is_ok());
    }
    #[test]
    fn consumed_batch_cannot_be_replayed_even_with_matching_fields() {
        let mut s = fixture();
        let batch = s.host_ready(context(), 1).unwrap();
        // Internal test deliberately reconstructs a batch; downstream callers
        // cannot construct it because all fields are private.
        let duplicate = Batch {
            context: batch.context,
            sequence: batch.sequence,
            effects: batch.effects.clone(),
        };
        assert!(s.consume_batch(batch, 1).is_ok());
        assert_eq!(s.consume_batch(duplicate, 1), Err(Error::Stale));
    }
    #[test]
    fn sequence_exhaustion_never_wraps_or_changes_reducer() {
        let mut s = fixture();
        s.sequence = u64::MAX;
        assert_eq!(
            s.host_ready(context(), 1).unwrap_err(),
            Error::SequenceExhausted
        );
        assert_eq!(s.phase(), &gestures::Phase::Mapping);
        assert!(!s.is_valid());
        assert!(s.pending.is_none());
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidContext,
    InvalidLease,
    Snapshot,
    Allocation,
    Stale,
    Expired,
    ClockRegression,
    HostOnlyEvent,
    SequenceExhausted,
    Reducer(gestures::Error),
}
#[derive(Debug)]
pub struct Batch {
    context: Context,
    sequence: u64,
    effects: Vec<gestures::Effect>,
}
impl Batch {
    pub fn context(&self) -> Context {
        self.context
    }
    pub fn effects(&self) -> &[gestures::Effect] {
        &self.effects
    }
}
pub struct Session {
    context: Context,
    deadline: u64,
    last_now: u64,
    valid: bool,
    sequence: u64,
    pending: Option<u64>,
    reducer: gestures::Reducer,
    snapshot: snapshot::Snapshot,
    allocation: allocation::Allocation,
}
impl Session {
    /// Context and absolute monotonic lease deadline come from the trusted
    /// arbiter adapter. Constructing a Session does not acquire a modal lease.
    pub fn open(
        context: Context,
        deadline: u64,
        now: u64,
        monitors: &[snapshot::Monitor],
        clients: &[snapshot::Client],
        remembered: &BTreeMap<String, String>,
        tap_ms: u64,
    ) -> Result<(Self, Batch), Error> {
        if context.presenter == [0; 16] || context.source_revision == 0 {
            return Err(Error::InvalidContext);
        }
        if deadline <= now || deadline - now > 5000 {
            return Err(Error::InvalidLease);
        }
        let snapshot = snapshot::normalize(monitors, clients).map_err(|_| Error::Snapshot)?;
        let visible = snapshot
            .windows
            .iter()
            .map(|w| w.hint_id.clone())
            .collect::<Vec<_>>();
        let allocation = allocation::allocate(&visible, &snapshot.live_ids, remembered)
            .map_err(|_| Error::Allocation)?;
        let hints = allocation
            .visible
            .iter()
            .map(|(id, hint)| (hint.clone(), id.clone()))
            .collect();
        let (reducer, effects) = gestures::Reducer::open(
            gestures::Generation(context.fence.generation.get()),
            hints,
            now,
            tap_ms,
        )
        .map_err(Error::Reducer)?;
        Ok((
            Self {
                context,
                deadline,
                last_now: now,
                valid: true,
                sequence: 1,
                pending: Some(1),
                reducer,
                snapshot,
                allocation,
            },
            Batch {
                context,
                sequence: 1,
                effects,
            },
        ))
    }
    pub fn context(&self) -> Context {
        self.context
    }
    pub fn snapshot(&self) -> &snapshot::Snapshot {
        &self.snapshot
    }
    pub fn allocation(&self) -> &allocation::Allocation {
        &self.allocation
    }
    pub fn phase(&self) -> &gestures::Phase {
        self.reducer.phase()
    }
    pub fn is_valid(&self) -> bool {
        self.valid
    }
    fn check(&mut self, context: Context, now: u64) -> Result<(), Error> {
        if !self.valid || context != self.context {
            return Err(Error::Stale);
        }
        if now < self.last_now {
            return Err(Error::ClockRegression);
        }
        self.last_now = now;
        if now >= self.deadline {
            self.valid = false;
            return Err(Error::Expired);
        }
        Ok(())
    }
    /// A UI map callback alone must never call this. The host has to verify the
    /// authenticated child channel and independent compositor layer evidence.
    pub fn host_ready(&mut self, context: Context, now: u64) -> Result<Batch, Error> {
        self.apply(context, gestures::Event::Mapped, now)
    }
    pub fn input(
        &mut self,
        context: Context,
        event: gestures::Event,
        now: u64,
    ) -> Result<Batch, Error> {
        if matches!(
            event,
            gestures::Event::Mapped | gestures::Event::HoldCompleted { .. }
        ) {
            return Err(Error::HostOnlyEvent);
        }
        self.apply(context, event, now)
    }
    pub fn host_completed(
        &mut self,
        context: Context,
        operation_id: u64,
        completed: bool,
        now: u64,
    ) -> Result<Batch, Error> {
        self.apply(
            context,
            gestures::Event::HoldCompleted {
                operation_id,
                completed,
            },
            now,
        )
    }
    fn apply(
        &mut self,
        context: Context,
        event: gestures::Event,
        now: u64,
    ) -> Result<Batch, Error> {
        self.check(context, now)?;
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.invalidate();
            return Err(Error::SequenceExhausted);
        };
        let effects = self
            .reducer
            .apply(
                gestures::Generation(context.fence.generation.get()),
                now,
                event,
            )
            .map_err(Error::Reducer)?;
        self.sequence = sequence;
        self.pending = Some(sequence);
        Ok(Batch {
            context,
            sequence,
            effects,
        })
    }
    /// Native disconnect, any source revision change, or lost lease invalidates
    /// the whole presentation. The arbiter owns reset/dismissal, not this object.
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.pending = None;
    }
    pub fn renew(&mut self, context: Context, deadline: u64, now: u64) -> Result<(), Error> {
        self.check(context, now)?;
        if deadline <= now || deadline - now > 5000 || deadline < self.deadline {
            return Err(Error::InvalidLease);
        }
        self.deadline = deadline;
        Ok(())
    }
    /// Consume immediately before synchronous forwarding. Every accepted reducer
    /// transition supersedes old batches, including Cancel and empty Tick output.
    /// No batch grants authority: the arbiter still validates registry and fence.
    pub fn consume_batch(
        &mut self,
        batch: Batch,
        now: u64,
    ) -> Result<Vec<gestures::Effect>, Error> {
        self.check(batch.context, now)?;
        if self.pending != Some(batch.sequence) {
            return Err(Error::Stale);
        }
        self.pending = None;
        Ok(batch.effects)
    }
}
