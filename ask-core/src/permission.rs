use crate::{Error, Id, RequestId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    AllowOnce,
    RejectOnce,
    AllowAlways,
    RejectAlways,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub id: Id,
    pub kind: Kind,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Selected(Id),
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub request: RequestId,
    pub outcome: Outcome,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Context {
    pub project: String,
    pub capability: Id,
}
#[derive(Clone, PartialEq, Eq)]
pub enum Policy {
    Ask,
    /// Explicit migrated choice. This never substitutes for OS/provider authorization.
    LegacyYolo,
    Scoped {
        project: String,
        capabilities: BTreeSet<Id>,
        expires_ms: u64,
    },
}
impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ask => "Ask",
            Self::LegacyYolo => "LegacyYolo",
            Self::Scoped { .. } => "Scoped(<redacted>)",
        })
    }
}
impl Policy {
    fn validate(&self, now: u64) -> Result<(), Error> {
        if let Self::Scoped {
            project,
            capabilities,
            expires_ms,
        } = self
        {
            if !project.starts_with('/')
                || project.len() > 4096
                || project.contains('\0')
                || capabilities.is_empty()
                || capabilities.len() > 32
            {
                return Err(Error::InvalidConfig);
            }
            if expires_ms.saturating_sub(now) > 86_400_000 {
                return Err(Error::InvalidConfig);
            }
            if *expires_ms <= now {
                return Err(Error::Expired);
            }
        }
        Ok(())
    }
    fn permits(&self, context: Option<&Context>, now: u64) -> bool {
        match self {
            Self::Ask => false,
            Self::LegacyYolo => true,
            Self::Scoped {
                project,
                capabilities,
                expires_ms,
            } => context.is_some_and(|c| {
                now < *expires_ms && c.project == *project && capabilities.contains(&c.capability)
            }),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitToken {
    pub generation: u64,
    pub operation: u64,
    pub expected_revision: u64,
    pub next_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistPolicy {
    pub token: CommitToken,
    pub policy: Policy,
}
struct Pending {
    options: Vec<Offered>,
    context: Option<Context>,
}
pub struct Broker {
    generation: u64,
    permission_epoch: u64,
    revision: u64,
    next_operation: u64,
    time: u64,
    policy: Policy,
    commit: Option<PersistPolicy>,
    pending: BTreeMap<RequestId, Pending>,
    order: VecDeque<RequestId>,
    seen: BTreeSet<RequestId>,
}
impl Broker {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            permission_epoch: 0,
            revision: 0,
            next_operation: 0,
            time: 0,
            policy: Policy::Ask,
            commit: None,
            pending: BTreeMap::new(),
            order: VecDeque::new(),
            seen: BTreeSet::new(),
        }
    }
    /// Only call after the IO owner has validated an explicit, durable existing policy.
    /// Caller must verify the persisted clock domain matches the trusted stable clock used
    /// for `now` (never process-relative Instant); unknown/mismatched domains restore Ask.
    /// Expired scoped policies restore as Ask; an explicit legacy choice is preserved.
    pub fn from_durable(
        generation: u64,
        revision: u64,
        policy: Policy,
        now: u64,
    ) -> Result<Self, Error> {
        let policy = match policy.validate(now) {
            Ok(()) => policy,
            Err(Error::Expired) => Policy::Ask,
            Err(e) => return Err(e),
        };
        Ok(Self {
            revision,
            policy,
            time: now,
            ..Self::new(generation)
        })
    }
    fn check(&self, generation: u64, now: u64) -> Result<(), Error> {
        if generation != self.generation {
            return Err(Error::StaleGeneration);
        }
        if now < self.time {
            return Err(Error::ClockWentBackwards);
        }
        Ok(())
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    /// Capture with the displayed request; never substitute the current epoch on click.
    pub fn permission_epoch(&self) -> u64 {
        self.permission_epoch
    }
    pub fn first_pending(&self) -> Option<&RequestId> {
        self.order.front()
    }
    pub fn request(
        &mut self,
        generation: u64,
        id: RequestId,
        options: Vec<Offered>,
        context: Option<Context>,
        now: u64,
    ) -> Result<Option<Response>, Error> {
        self.check(generation, now)?;
        if self.seen.contains(&id) {
            return Err(Error::Duplicate);
        }
        if options.len() > 32 || self.seen.len() >= 256 {
            return Err(Error::LimitExceeded);
        }
        if options.iter().map(|o| &o.id).collect::<BTreeSet<_>>().len() != options.len() {
            return Err(Error::Duplicate);
        }
        if context.as_ref().is_some_and(|c| {
            !c.project.starts_with('/') || c.project.len() > 4096 || c.project.contains('\0')
        }) {
            return Err(Error::InvalidInput);
        }
        let automatic = self.commit.is_none() && self.policy.permits(context.as_ref(), now);
        if !automatic && self.pending.len() >= 32 {
            return Err(Error::LimitExceeded);
        }
        self.time = now;
        self.seen.insert(id.clone());
        if automatic {
            return Ok(Some(Response {
                request: id,
                outcome: once(&options, true),
            }));
        }
        self.order.push_back(id.clone());
        self.pending.insert(id, Pending { options, context });
        Ok(None)
    }
    pub fn answer(
        &mut self,
        generation: u64,
        permission_epoch: u64,
        id: &RequestId,
        selected: Option<&Id>,
    ) -> Result<Response, Error> {
        self.check(generation, self.time)?;
        if permission_epoch != self.permission_epoch {
            return Err(Error::StaleOperation);
        }
        let pending = self.pending.get(id).ok_or(Error::UnknownRequest)?;
        if selected.is_some_and(|s| !pending.options.iter().any(|o| &o.id == s)) {
            return Err(Error::UnknownOption);
        }
        let outcome = selected
            .cloned()
            .map(Outcome::Selected)
            .unwrap_or(Outcome::Cancelled);
        self.pending.remove(id);
        self.order.retain(|x| x != id);
        Ok(Response {
            request: id.clone(),
            outcome,
        })
    }
    pub fn answer_once(
        &mut self,
        generation: u64,
        permission_epoch: u64,
        id: &RequestId,
        allow: bool,
    ) -> Result<Response, Error> {
        self.check(generation, self.time)?;
        if permission_epoch != self.permission_epoch {
            return Err(Error::StaleOperation);
        }
        let pending = self.pending.get(id).ok_or(Error::UnknownRequest)?;
        let selected = match once(&pending.options, allow) {
            Outcome::Selected(id) => Some(id),
            Outcome::Cancelled => None,
        };
        self.answer(generation, permission_epoch, id, selected.as_ref())
    }
    pub fn propose(
        &mut self,
        generation: u64,
        expected_revision: u64,
        policy: Policy,
        now: u64,
    ) -> Result<PersistPolicy, Error> {
        self.check(generation, now)?;
        policy.validate(now)?;
        if self.commit.is_some() {
            return Err(Error::PolicyPending);
        }
        if self.revision != expected_revision {
            return Err(Error::StaleRevision);
        }
        let next_revision = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let operation = self.next_operation.checked_add(1).ok_or(Error::Exhausted)?;
        let pending = PersistPolicy {
            token: CommitToken {
                generation,
                operation,
                expected_revision,
                next_revision,
            },
            policy,
        };
        self.next_operation = operation;
        self.time = now;
        self.commit = Some(pending.clone());
        Ok(pending)
    }
    /// A successful ack must mean durable file+parent commit by the trusted writer. A rename
    /// with uncertain durability must NOT be passed as success. Failure keeps old policy and queue.
    pub fn committed(
        &mut self,
        token: CommitToken,
        success: bool,
        now: u64,
    ) -> Result<Vec<Response>, Error> {
        self.check(token.generation, now)?;
        let commit = self.commit.as_ref().ok_or(Error::StaleOperation)?;
        if commit.token != token {
            return Err(Error::StaleOperation);
        }
        self.time = now;
        let commit = self.commit.take().expect("checked transaction");
        if !success {
            return Err(Error::PersistenceFailed);
        }
        self.policy = commit.policy;
        self.revision = token.next_revision;
        let mut responses = Vec::new();
        for id in self.order.iter().cloned().collect::<Vec<_>>() {
            let p = &self.pending[&id];
            if self.policy.permits(p.context.as_ref(), now) {
                let outcome = once(&p.options, true);
                self.pending.remove(&id);
                self.order.retain(|x| x != &id);
                responses.push(Response {
                    request: id,
                    outcome,
                });
            }
        }
        Ok(responses)
    }
    pub fn cancel_pending(&mut self) -> Vec<Response> {
        let out = self
            .order
            .drain(..)
            .map(|request| Response {
                request,
                outcome: Outcome::Cancelled,
            })
            .collect();
        self.pending.clear();
        out
    }
    /// New turn after prior cancellation/completion; never called while old work is live.
    pub fn next_turn(&mut self) -> Result<(), Error> {
        if !self.pending.is_empty() {
            return Err(Error::Busy);
        }
        self.permission_epoch = self
            .permission_epoch
            .checked_add(1)
            .ok_or(Error::Exhausted)?;
        self.seen.clear();
        Ok(())
    }
    pub fn abort(&mut self) -> Vec<Response> {
        self.commit = None;
        self.cancel_pending()
    }
    pub fn reset_generation(&mut self, generation: u64) -> Result<Vec<Response>, Error> {
        if generation <= self.generation {
            return Err(Error::StaleGeneration);
        }
        let out = self.abort();
        self.generation = generation;
        self.seen.clear();
        Ok(out)
    }
}
fn once(options: &[Offered], allow: bool) -> Outcome {
    let kind = if allow {
        Kind::AllowOnce
    } else {
        Kind::RejectOnce
    };
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| Outcome::Selected(o.id.clone()))
        .unwrap_or(Outcome::Cancelled)
}
