//! Deterministic, bounded modal-ownership contract. No I/O, clock, or cross-process lock.
//!
//! One external authority must serialize calls and authenticate owners. A successful
//! action check is a point-in-time decision, not a durable capability: the effect
//! adapter must enforce the fence and expiry at the actual effect boundary.

use std::num::NonZeroU64;

/// Authority-instance namespace. The deployment must never reuse an epoch after restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorityEpoch(pub [u8; 16]);

/// Authenticated, stable application identity; never a window title or PID alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub client: ClientId,
    pub client_epoch: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestKey {
    pub owner: Owner,
    pub request_id: NonZeroU64,
}

/// Match the entire fence, including authority epoch and owner, never generation alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub authority_epoch: AuthorityEpoch,
    pub generation: NonZeroU64,
    pub owner: Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub fence: Fence,
    /// Exclusive deadline in adapter-defined monotonic ticks. Valid iff now < expiry.
    pub expires_at: u64,
    /// Request that caused the grant (the old owner for an atomic handoff).
    pub granted_by: RequestKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Acquire {
        ttl: u64,
    },
    Renew {
        fence: Fence,
        ttl: u64,
    },
    Release {
        fence: Fence,
    },
    /// Immediate transfer to an already connected recipient; not an expiring offer.
    Handoff {
        fence: Fence,
        target: Owner,
        ttl: u64,
    },
    ValidateAction {
        fence: Fence,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Granted(Lease),
    Renewed(Lease),
    Released,
    ActionValid(Lease),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidConfig,
    ClientCapacity,
    StaleIdentity,
    DuplicateOrOldRequest,
    ClockWentBackwards,
    InvalidTtl,
    DeadlineOverflow,
    GenerationExhausted,
    Busy,
    StaleFence,
    InvalidHandoff,
}

#[derive(Clone, Copy, Debug)]
struct Client {
    owner: Owner,
    connected: bool,
    last_request: u64,
}

/// Hard cap protects against accidentally unbounded allocation at construction.
pub const HARD_CLIENT_LIMIT: usize = 256;

/// Single-authority reducer. The `&mut self` API serializes one instance, not other
/// instances or other processes. See README for the deployment contract.
pub struct Authority {
    epoch: AuthorityEpoch,
    max_clients: usize,
    max_ttl: NonZeroU64,
    clients: Vec<Client>,
    lease: Option<Lease>,
    last_now: u64,
    last_generation: u64,
}

impl Authority {
    pub fn new(
        epoch: AuthorityEpoch,
        max_clients: usize,
        max_ttl: NonZeroU64,
        now: u64,
    ) -> Result<Self, Error> {
        if max_clients == 0 || max_clients > HARD_CLIENT_LIMIT {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            epoch,
            max_clients,
            max_ttl,
            clients: Vec::with_capacity(max_clients),
            lease: None,
            last_now: now,
            last_generation: 0,
        })
    }

    /// Register a trusted connection. Reconnect epochs must strictly increase.
    /// Slots are deliberately never evicted: remembering epochs prevents replay.
    /// At capacity, new identities fail closed until an authority restart with a
    /// fresh epoch. Existing identities can reconnect without another allocation.
    pub fn connect(&mut self, owner: Owner, now: u64) -> Result<(), Error> {
        self.advance(now)?;
        if let Some(i) = self
            .clients
            .iter()
            .position(|c| c.owner.client == owner.client)
        {
            if owner.client_epoch <= self.clients[i].owner.client_epoch {
                return Err(Error::StaleIdentity);
            }
            if self
                .lease
                .is_some_and(|l| l.fence.owner.client == owner.client)
            {
                self.lease = None;
            }
            self.clients[i] = Client {
                owner,
                connected: true,
                last_request: 0,
            };
        } else {
            if self.clients.len() >= self.max_clients {
                return Err(Error::ClientCapacity);
            }
            self.clients.push(Client {
                owner,
                connected: true,
                last_request: 0,
            });
        }
        Ok(())
    }

    /// Authenticated disconnect revokes immediately. Missing crash notification
    /// is handled by expiry when advance/snapshot/another command is called.
    pub fn disconnect(&mut self, owner: Owner, now: u64) -> Result<(), Error> {
        self.advance(now)?;
        let i = self.client_index(owner)?;
        self.clients[i].connected = false;
        if self.lease.is_some_and(|l| l.fence.owner == owner) {
            self.lease = None;
        }
        Ok(())
    }

    /// Expiry must also drive the real UI's dismissal via the effect adapter.
    /// No work or timer is scheduled by this pure library.
    pub fn advance(&mut self, now: u64) -> Result<(), Error> {
        if now < self.last_now {
            return Err(Error::ClockWentBackwards);
        }
        self.last_now = now;
        if self.lease.is_some_and(|l| now >= l.expires_at) {
            self.lease = None;
        }
        Ok(())
    }

    pub fn snapshot(&mut self, now: u64) -> Result<Option<Lease>, Error> {
        self.advance(now)?;
        Ok(self.lease)
    }

    /// Request IDs strictly increase within a client epoch. Even a business error
    /// (e.g. Busy) consumes the ID. Retries query a snapshot and use a new ID;
    /// this contract never replays an expired grant from an idempotency cache.
    pub fn apply(&mut self, key: RequestKey, command: Command, now: u64) -> Result<Reply, Error> {
        self.advance(now)?;
        let i = self.client_index(key.owner)?;
        if key.request_id.get() <= self.clients[i].last_request {
            return Err(Error::DuplicateOrOldRequest);
        }
        self.clients[i].last_request = key.request_id.get();
        match command {
            Command::Acquire { ttl } => {
                let deadline = self.deadline(now, ttl)?;
                if self.lease.is_some() {
                    return Err(Error::Busy);
                }
                let lease = self.grant(key.owner, key, deadline)?;
                Ok(Reply::Granted(lease))
            }
            Command::Renew { fence, ttl } => {
                let mut lease = self.check(key.owner, fence)?;
                let deadline = self.deadline(now, ttl)?;
                lease.expires_at = deadline.max(lease.expires_at);
                self.lease = Some(lease);
                Ok(Reply::Renewed(lease))
            }
            Command::Release { fence } => {
                self.check(key.owner, fence)?;
                self.lease = None;
                Ok(Reply::Released)
            }
            Command::Handoff { fence, target, ttl } => {
                self.check(key.owner, fence)?;
                self.client_index(target)?;
                if target == key.owner {
                    return Err(Error::InvalidHandoff);
                }
                let deadline = self.deadline(now, ttl)?;
                // All checks precede mutation. A failed handoff preserves old lease.
                let lease = self.grant(target, key, deadline)?;
                Ok(Reply::Granted(lease))
            }
            Command::ValidateAction { fence } => {
                Ok(Reply::ActionValid(self.check(key.owner, fence)?))
            }
        }
    }

    fn client_index(&self, owner: Owner) -> Result<usize, Error> {
        self.clients
            .iter()
            .position(|c| c.connected && c.owner == owner)
            .ok_or(Error::StaleIdentity)
    }

    fn check(&self, owner: Owner, fence: Fence) -> Result<Lease, Error> {
        self.lease
            .filter(|l| l.fence == fence && fence.owner == owner)
            .ok_or(Error::StaleFence)
    }

    fn deadline(&self, now: u64, ttl: u64) -> Result<u64, Error> {
        if ttl == 0 {
            return Err(Error::InvalidTtl);
        }
        now.checked_add(ttl.min(self.max_ttl.get()))
            .ok_or(Error::DeadlineOverflow)
    }

    fn grant(&mut self, owner: Owner, key: RequestKey, expires_at: u64) -> Result<Lease, Error> {
        let generation = self
            .last_generation
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(Error::GenerationExhausted)?;
        let lease = Lease {
            fence: Fence {
                authority_epoch: self.epoch,
                generation,
                owner,
            },
            expires_at,
            granted_by: key,
        };
        self.last_generation = generation.get();
        self.lease = Some(lease);
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn nz(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).unwrap()
    }
    fn owner(id: u8, epoch: u64) -> Owner {
        Owner {
            client: ClientId([id; 16]),
            client_epoch: nz(epoch),
        }
    }
    fn key(o: Owner, n: u64) -> RequestKey {
        RequestKey {
            owner: o,
            request_id: nz(n),
        }
    }
    fn authority() -> Authority {
        Authority::new(AuthorityEpoch([9; 16]), 2, nz(100), 0).unwrap()
    }
    fn acquire(a: &mut Authority, o: Owner, id: u64, now: u64, ttl: u64) -> Lease {
        match a.apply(key(o, id), Command::Acquire { ttl }, now).unwrap() {
            Reply::Granted(l) => l,
            _ => panic!("expected grant"),
        }
    }
    fn pair() -> (Authority, Owner, Owner) {
        let (a, b) = (owner(1, 1), owner(2, 1));
        let mut auth = authority();
        auth.connect(a, 0).unwrap();
        auth.connect(b, 0).unwrap();
        (auth, a, b)
    }

    #[test]
    fn ordered_simultaneous_acquisitions_have_one_winner_in_either_order() {
        for reverse in [false, true] {
            let (mut a, x, y) = pair();
            let (first, second) = if reverse { (y, x) } else { (x, y) };
            let l = acquire(&mut a, first, 1, 5, 10);
            assert_eq!(
                a.apply(key(second, 1), Command::Acquire { ttl: 10 }, 5),
                Err(Error::Busy)
            );
            assert_eq!(a.snapshot(5).unwrap(), Some(l));
        }
    }
    #[test]
    fn expiry_is_exclusive_and_crash_needs_no_release() {
        let (mut a, x, y) = pair();
        let old = acquire(&mut a, x, 1, 0, 10);
        assert_eq!(a.snapshot(9).unwrap(), Some(old));
        let new = acquire(&mut a, y, 1, 10, 10);
        assert!(new.fence.generation > old.fence.generation);
        for (id, c) in [
            (2, Command::Release { fence: old.fence }),
            (
                3,
                Command::Renew {
                    fence: old.fence,
                    ttl: 10,
                },
            ),
            (4, Command::ValidateAction { fence: old.fence }),
        ] {
            assert_eq!(a.apply(key(x, id), c, 10), Err(Error::StaleFence));
        }
        assert_eq!(a.snapshot(10).unwrap(), Some(new));
    }
    #[test]
    fn reconnect_revokes_previous_epoch_and_old_disconnect_cannot_revoke_new() {
        let (mut a, x, _) = pair();
        let old = acquire(&mut a, x, 1, 0, 100);
        let new_owner = owner(1, 2);
        a.connect(new_owner, 1).unwrap();
        let new = acquire(&mut a, new_owner, 1, 1, 100);
        assert_eq!(a.disconnect(x, 2), Err(Error::StaleIdentity));
        assert_eq!(
            a.apply(key(x, 2), Command::Release { fence: old.fence }, 2),
            Err(Error::StaleIdentity)
        );
        assert_eq!(
            a.apply(
                key(new_owner, 2),
                Command::ValidateAction { fence: old.fence },
                2
            ),
            Err(Error::StaleFence)
        );
        assert_eq!(a.snapshot(2).unwrap(), Some(new));
    }
    #[test]
    fn request_replay_never_reexecutes_a_grant() {
        let (mut a, x, _) = pair();
        let l = acquire(&mut a, x, 7, 0, 2);
        assert_eq!(
            a.apply(key(x, 7), Command::Acquire { ttl: 2 }, 2),
            Err(Error::DuplicateOrOldRequest)
        );
        assert_eq!(a.snapshot(2).unwrap(), None);
        assert_eq!(
            a.apply(key(x, 8), Command::Release { fence: l.fence }, 2),
            Err(Error::StaleFence)
        );
    }
    #[test]
    fn business_errors_consume_request_ids() {
        let (mut a, x, y) = pair();
        acquire(&mut a, x, 1, 0, 10);
        assert_eq!(
            a.apply(key(y, 3), Command::Acquire { ttl: 10 }, 0),
            Err(Error::Busy)
        );
        assert_eq!(
            a.apply(key(y, 3), Command::Acquire { ttl: 10 }, 10),
            Err(Error::DuplicateOrOldRequest)
        );
        acquire(&mut a, y, 4, 10, 10);
    }
    #[test]
    fn renewal_clamps_and_does_not_shorten() {
        let (mut a, x, _) = pair();
        let l = acquire(&mut a, x, 1, 0, 1000);
        assert_eq!(l.expires_at, 100);
        assert!(matches!(
            a.apply(
                key(x, 2),
                Command::Renew {
                    fence: l.fence,
                    ttl: 1
                },
                2
            ),
            Ok(Reply::Renewed(Lease {
                expires_at: 100,
                ..
            }))
        ));
        assert!(matches!(
            a.apply(
                key(x, 3),
                Command::Renew {
                    fence: l.fence,
                    ttl: u64::MAX
                },
                3
            ),
            Ok(Reply::Renewed(Lease {
                expires_at: 103,
                ..
            }))
        ));
    }
    #[test]
    fn handoff_is_atomic_and_rotates_fence_then_expires() {
        let (mut a, x, y) = pair();
        let l = acquire(&mut a, x, 1, 0, 100);
        let Reply::Granted(new) = a
            .apply(
                key(x, 2),
                Command::Handoff {
                    fence: l.fence,
                    target: y,
                    ttl: 5,
                },
                1,
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(new.fence.owner, y);
        assert!(new.fence.generation > l.fence.generation);
        assert_eq!(
            a.apply(key(x, 3), Command::Release { fence: l.fence }, 2),
            Err(Error::StaleFence)
        );
        assert!(matches!(
            a.apply(key(y, 1), Command::ValidateAction { fence: new.fence }, 5),
            Ok(Reply::ActionValid(_))
        ));
        assert_eq!(
            a.apply(key(y, 2), Command::ValidateAction { fence: new.fence }, 6),
            Err(Error::StaleFence)
        );
        assert_eq!(a.snapshot(6).unwrap(), None);
    }
    #[test]
    fn handoff_at_expiry_fails_and_invalid_target_preserves_owner() {
        let (mut a, x, y) = pair();
        let l = acquire(&mut a, x, 1, 0, 5);
        assert_eq!(
            a.apply(
                key(x, 2),
                Command::Handoff {
                    fence: l.fence,
                    target: owner(2, 2),
                    ttl: 1
                },
                1
            ),
            Err(Error::StaleIdentity)
        );
        assert_eq!(a.snapshot(1).unwrap(), Some(l));
        assert_eq!(
            a.apply(
                key(x, 3),
                Command::Handoff {
                    fence: l.fence,
                    target: y,
                    ttl: 1
                },
                5
            ),
            Err(Error::StaleFence)
        );
    }
    #[test]
    fn deadline_overflow_is_rejected_without_releasing_old_owner() {
        let (mut a, x, y) = pair();
        let l = acquire(&mut a, x, 1, u64::MAX - 5, 4);
        assert_eq!(
            a.apply(
                key(x, 2),
                Command::Handoff {
                    fence: l.fence,
                    target: y,
                    ttl: 100
                },
                u64::MAX - 4
            ),
            Err(Error::DeadlineOverflow)
        );
        assert_eq!(a.snapshot(u64::MAX - 4).unwrap(), Some(l));
        assert_eq!(
            a.apply(
                key(x, 3),
                Command::Renew {
                    fence: l.fence,
                    ttl: 100
                },
                u64::MAX - 4
            ),
            Err(Error::DeadlineOverflow)
        );
        assert_eq!(a.snapshot(u64::MAX).unwrap(), None);
        assert_eq!(
            a.apply(key(x, 4), Command::Acquire { ttl: 1 }, u64::MAX),
            Err(Error::DeadlineOverflow)
        );
    }
    #[test]
    fn exhausted_generation_never_wraps_or_destroys_owner_on_handoff() {
        let (mut a, x, y) = pair();
        a.last_generation = u64::MAX - 1;
        let l = acquire(&mut a, x, 1, 0, 2);
        assert_eq!(l.fence.generation.get(), u64::MAX);
        assert_eq!(
            a.apply(
                key(x, 2),
                Command::Handoff {
                    fence: l.fence,
                    target: y,
                    ttl: 1
                },
                0
            ),
            Err(Error::GenerationExhausted)
        );
        assert_eq!(a.snapshot(0).unwrap(), Some(l));
        assert_eq!(
            a.apply(key(y, 1), Command::Acquire { ttl: 1 }, 2),
            Err(Error::GenerationExhausted)
        );
        assert_eq!(a.last_generation, u64::MAX);
    }
    #[test]
    fn exhausted_request_id_requires_new_client_epoch() {
        let (mut a, x, _) = pair();
        let l = acquire(&mut a, x, u64::MAX, 0, 10);
        assert_eq!(
            a.apply(key(x, 1), Command::Release { fence: l.fence }, 1),
            Err(Error::DuplicateOrOldRequest)
        );
        let next = owner(1, 2);
        a.connect(next, 1).unwrap();
        acquire(&mut a, next, 1, 1, 10);
    }
    #[test]
    fn clock_regression_is_rejected_without_consuming_request() {
        let (mut a, x, _) = pair();
        a.advance(10).unwrap();
        assert_eq!(
            a.apply(key(x, 1), Command::Acquire { ttl: 10 }, 9),
            Err(Error::ClockWentBackwards)
        );
        acquire(&mut a, x, 1, 10, 10);
    }
    #[test]
    fn wrong_client_and_wrong_authority_cannot_use_current_fence() {
        let (mut a, x, y) = pair();
        let l = acquire(&mut a, x, 1, 0, 10);
        assert_eq!(
            a.apply(key(y, 1), Command::ValidateAction { fence: l.fence }, 1),
            Err(Error::StaleFence)
        );
        let mut wrong = l.fence;
        wrong.authority_epoch = AuthorityEpoch([8; 16]);
        assert_eq!(
            a.apply(key(x, 2), Command::ValidateAction { fence: wrong }, 1),
            Err(Error::StaleFence)
        );
    }
    #[test]
    fn authority_restart_with_fresh_epoch_invalidates_prior_fences() {
        let (mut old, x, _) = pair();
        let l = acquire(&mut old, x, 1, 0, 10);
        let mut new = Authority::new(AuthorityEpoch([8; 16]), 2, nz(100), 0).unwrap();
        new.connect(x, 0).unwrap();
        let current = acquire(&mut new, x, 1, 0, 10);
        assert_eq!(current.fence.generation, l.fence.generation);
        assert_eq!(
            new.apply(key(x, 2), Command::Release { fence: l.fence }, 1),
            Err(Error::StaleFence)
        );
    }
    #[test]
    fn disconnected_slots_are_bounded_and_not_recycled_for_replay() {
        let (mut a, x, y) = pair();
        a.disconnect(x, 1).unwrap();
        assert_eq!(a.connect(owner(3, 1), 1), Err(Error::ClientCapacity));
        assert_eq!(a.connect(x, 1), Err(Error::StaleIdentity));
        assert_eq!(
            a.apply(key(x, 1), Command::Acquire { ttl: 1 }, 1),
            Err(Error::StaleIdentity)
        );
        a.connect(owner(1, 2), 1).unwrap();
        acquire(&mut a, y, 1, 1, 1);
    }
    #[test]
    fn invalid_ttl_and_self_handoff_leave_active_lease_unchanged() {
        let (mut a, x, _) = pair();
        assert_eq!(
            a.apply(key(x, 1), Command::Acquire { ttl: 0 }, 0),
            Err(Error::InvalidTtl)
        );
        let l = acquire(&mut a, x, 2, 0, 10);
        assert_eq!(
            a.apply(
                key(x, 3),
                Command::Renew {
                    fence: l.fence,
                    ttl: 0
                },
                1
            ),
            Err(Error::InvalidTtl)
        );
        assert_eq!(
            a.apply(
                key(x, 4),
                Command::Handoff {
                    fence: l.fence,
                    target: x,
                    ttl: 1
                },
                1
            ),
            Err(Error::InvalidHandoff)
        );
        assert_eq!(a.snapshot(1).unwrap(), Some(l));
    }
    #[test]
    fn maximum_client_epoch_cannot_be_reused() {
        let mut a = authority();
        let x = owner(1, u64::MAX);
        a.connect(x, 0).unwrap();
        assert_eq!(a.connect(owner(1, 1), 0), Err(Error::StaleIdentity));
        assert_eq!(a.connect(x, 0), Err(Error::StaleIdentity));
    }
    #[test]
    fn config_has_hard_allocation_bounds() {
        for count in [0, HARD_CLIENT_LIMIT + 1, usize::MAX] {
            assert!(matches!(
                Authority::new(AuthorityEpoch([0; 16]), count, nz(1), 0),
                Err(Error::InvalidConfig)
            ));
        }
    }
    #[test]
    fn successful_release_and_disconnect_revoke_without_waiting_for_expiry() {
        let (mut a, x, y) = pair();
        let old = acquire(&mut a, x, 1, 0, 100);
        assert_eq!(
            a.apply(key(x, 2), Command::Release { fence: old.fence }, 1),
            Ok(Reply::Released)
        );
        assert_eq!(a.snapshot(1).unwrap(), None);
        let next = acquire(&mut a, y, 1, 1, 100);
        assert!(next.fence.generation > old.fence.generation);
        a.disconnect(y, 2).unwrap();
        assert_eq!(a.snapshot(2).unwrap(), None);
        assert_eq!(
            a.apply(key(y, 2), Command::ValidateAction { fence: next.fence }, 2),
            Err(Error::StaleIdentity)
        );
    }
}
