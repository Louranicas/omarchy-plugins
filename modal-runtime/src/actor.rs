use crate::targets::{FocusIntent, NativeTarget, Page, Registry, SourceTicket};
use modal_contract::{Authority, AuthorityEpoch, Command, Fence, Lease, Owner, Reply, RequestKey};
use std::num::NonZeroU64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Application {
    Vimarchy,
    Ask,
    Yoohoo,
}
/// Closed modal operations; no raw dispatch string, argv, or reusable permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    EnterVimarchy,
    EnterVimarchyDouble,
    EnterAsk,
    EnterYoohoo,
    Reset,
}
#[derive(Clone, Copy, Debug)]
pub struct Dismissal {
    pub presenter_exited: bool,
    pub compositor_dismissed: bool,
}
/// Adapter calls must finish within their own bounded lifecycle policy. This
/// actor cannot forcibly interrupt a blocked adapter or infer native readback.
pub trait Backend {
    fn set_clock_origin(&mut self, _origin: std::time::Instant) {}
    fn register_controller(&mut self, _owner: Owner, _pid: u32) -> Result<(), Error> {
        Ok(())
    }
    fn presenter_identity(
        &mut self,
        _fence: Fence,
    ) -> Result<Option<crate::backend::PresenterIdentity>, Error> {
        Ok(None)
    }
    /// Trusted host registration; default refuses unidentified peers.
    fn register(&mut self, _owner: Owner, _app: Application) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn move_workspace(
        &mut self,
        _fence: Fence,
        _expires_at: u64,
        _target: &NativeTarget,
        _expected: &crate::workspace_move::MoveWorkspaceRequest,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn maximize(
        &mut self,
        _fence: Fence,
        _expires_at: u64,
        _target: &NativeTarget,
        _expected: &crate::vimarchy_action::MaximizeRequest,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn focus(
        &mut self,
        _fence: Fence,
        _expires_at: u64,
        _target: &NativeTarget,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn present(&mut self, fence: Fence, expires_at: u64) -> Result<(), Error>;
    fn dismiss(&mut self, fence: Fence) -> Result<Dismissal, Error>;
    fn dispatch(&mut self, fence: Fence, expires_at: u64, effect: Effect) -> Result<(), Error>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Contract(modal_contract::Error),
    Unavailable,
    IncompleteDismissal,
    Target(crate::targets::Error),
    EffectUnconfirmed,
}
impl From<modal_contract::Error> for Error {
    fn from(value: modal_contract::Error) -> Self {
        Self::Contract(value)
    }
}

/// A single serialized authority must own this actor and its backend. Socket
/// peers cannot choose another connection's Owner; the transport host mints it.
pub struct Actor<B> {
    authority: Authority,
    backend: B,
    active: Option<Lease>,
    unavailable: bool,
    targets: Registry,
}
impl<B: Backend> Actor<B> {
    pub fn new(epoch: AuthorityEpoch, backend: B, now: u64) -> Result<Self, Error> {
        Ok(Self {
            authority: Authority::new(epoch, 32, NonZeroU64::new(5000).unwrap(), now)?,
            backend,
            active: None,
            unavailable: false,
            targets: Registry::new(epoch.0),
        })
    }
    pub fn register_controller(&mut self, owner: Owner, pid: u32) -> Result<(), Error> {
        self.backend.register_controller(owner, pid)
    }
    pub fn presenter_identity(
        &mut self,
        fence: Fence,
    ) -> Result<Option<crate::backend::PresenterIdentity>, Error> {
        if self.unavailable || self.active.is_none_or(|lease| lease.fence != fence) {
            return Err(Error::Unavailable);
        }
        self.backend.presenter_identity(fence)
    }
    pub fn connect_application(
        &mut self,
        owner: Owner,
        app: Application,
        now: u64,
    ) -> Result<(), Error> {
        self.connect(owner, now)?;
        if let Err(error) = self.backend.register(owner, app) {
            self.authority.disconnect(owner, now)?;
            return Err(error);
        }
        Ok(())
    }
    pub fn connect(&mut self, owner: Owner, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        // Reconnecting an active identity must dismiss the old presenter first.
        self.authority.connect(owner, now)?;
        if self
            .active
            .is_some_and(|lease| lease.fence.owner.client == owner.client)
        {
            self.dismiss()?;
        }
        Ok(())
    }
    pub fn tick(&mut self, now: u64) -> Result<(), Error> {
        if self.unavailable {
            return Err(Error::Unavailable);
        }
        if self.active.is_some_and(|lease| now >= lease.expires_at) {
            self.dismiss()?;
        }
        self.authority.advance(now)?;
        Ok(())
    }
    fn dismiss(&mut self) -> Result<(), Error> {
        self.targets.revoke_view();
        if let Some(lease) = self.active {
            self.unavailable = true;
            let proof = self.backend.dismiss(lease.fence)?;
            if !proof.presenter_exited || !proof.compositor_dismissed {
                return Err(Error::IncompleteDismissal);
            }
            self.active = None;
            self.unavailable = false;
        }
        Ok(())
    }
    pub fn acquire(&mut self, key: RequestKey, ttl: u64, now: u64) -> Result<Lease, Error> {
        self.tick(now)?;
        let Reply::Granted(lease) = self.authority.apply(key, Command::Acquire { ttl }, now)?
        else {
            unreachable!()
        };
        self.targets.revoke_view();
        self.active = Some(lease);
        if let Err(error) = self.backend.present(lease.fence, lease.expires_at) {
            self.unavailable = true;
            return Err(error);
        }
        Ok(lease)
    }
    pub fn renew(
        &mut self,
        key: RequestKey,
        fence: Fence,
        ttl: u64,
        now: u64,
    ) -> Result<Lease, Error> {
        self.tick(now)?;
        let Reply::Renewed(lease) =
            self.authority
                .apply(key, Command::Renew { fence, ttl }, now)?
        else {
            unreachable!()
        };
        self.active = Some(lease);
        Ok(lease)
    }
    pub fn release(&mut self, key: RequestKey, fence: Fence, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        self.authority.apply(key, Command::Release { fence }, now)?;
        self.dismiss()?;
        Ok(())
    }
    pub fn disconnect(&mut self, owner: Owner, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        if self.active.is_some_and(|lease| lease.fence.owner == owner) {
            self.dismiss()?;
        }
        self.authority.disconnect(owner, now)?;
        Ok(())
    }
    /// Retry dismissal even after an adapter failure. A shutdown actor never
    /// grants again; retain socket/singleton custody until this returns success.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        let result = self.dismiss();
        self.unavailable = true;
        result
    }
    /// Trusted in-process observer callbacks only; never exposed as client commands.
    pub fn begin_source(&mut self) -> Result<SourceTicket, Error> {
        self.targets.begin_source().map_err(Error::Target)
    }
    pub fn snapshot(
        &mut self,
        ticket: SourceTicket,
        sequence: u64,
        targets: Vec<NativeTarget>,
    ) -> Result<(), Error> {
        self.targets
            .snapshot(ticket, sequence, targets)
            .map_err(Error::Target)
    }
    pub fn closed(
        &mut self,
        ticket: SourceTicket,
        sequence: u64,
        target: &NativeTarget,
    ) -> Result<(), Error> {
        self.targets
            .closed(ticket, sequence, target)
            .map_err(Error::Target)
    }
    pub fn lost(&mut self, ticket: SourceTicket, sequence: u64) -> Result<(), Error> {
        self.targets.lost(ticket, sequence).map_err(Error::Target)
    }
    pub fn list_targets(
        &mut self,
        key: RequestKey,
        fence: Fence,
        session: [u8; 16],
        cursor: usize,
        now: u64,
    ) -> Result<Page, Error> {
        self.tick(now)?;
        self.authority
            .apply(key, Command::ValidateAction { fence }, now)?;
        self.targets
            .page(fence, session, cursor)
            .map_err(Error::Target)
    }
    pub fn move_workspace(
        &mut self,
        key: RequestKey,
        fence: Fence,
        intent: &FocusIntent,
        expected: &crate::workspace_move::MoveWorkspaceRequest,
        now: u64,
    ) -> Result<(), Error> {
        self.tick(now)?;
        self.authority
            .apply(key, Command::ValidateAction { fence }, now)?;
        let target = self.targets.resolve(fence, intent).map_err(Error::Target)?;
        if let Err(error) = self.backend.move_workspace(
            fence,
            self.active.ok_or(Error::Unavailable)?.expires_at,
            &target,
            expected,
        ) {
            self.unavailable = true;
            self.targets.revoke_view();
            return Err(error);
        }
        Ok(())
    }
    pub fn maximize_window(
        &mut self,
        key: RequestKey,
        fence: Fence,
        intent: &FocusIntent,
        expected: &crate::vimarchy_action::MaximizeRequest,
        now: u64,
    ) -> Result<(), Error> {
        self.tick(now)?;
        self.authority
            .apply(key, Command::ValidateAction { fence }, now)?;
        let target = self.targets.resolve(fence, intent).map_err(Error::Target)?;
        if let Err(error) = self.backend.maximize(
            fence,
            self.active.ok_or(Error::Unavailable)?.expires_at,
            &target,
            expected,
        ) {
            self.unavailable = true;
            self.targets.revoke_view();
            return Err(error);
        }
        Ok(())
    }
    pub fn focus_window(
        &mut self,
        key: RequestKey,
        fence: Fence,
        intent: &FocusIntent,
        now: u64,
    ) -> Result<(), Error> {
        self.tick(now)?;
        self.authority
            .apply(key, Command::ValidateAction { fence }, now)?;
        let target = self.targets.resolve(fence, intent).map_err(Error::Target)?;
        if let Err(error) = self.backend.focus(
            fence,
            self.active.ok_or(Error::Unavailable)?.expires_at,
            &target,
        ) {
            // A submitted native effect may have happened even if readback failed.
            self.unavailable = true;
            self.targets.revoke_view();
            return Err(error);
        }
        Ok(())
    }
    pub fn execute(
        &mut self,
        key: RequestKey,
        fence: Fence,
        effect: Effect,
        now: u64,
    ) -> Result<(), Error> {
        self.tick(now)?;
        self.authority
            .apply(key, Command::ValidateAction { fence }, now)?;
        // No lease/permit escapes this method between validation and dispatch.
        // Backend must recheck expiry at actual compositor submission if it queues.
        if let Err(error) = self.backend.dispatch(
            fence,
            self.active.ok_or(Error::Unavailable)?.expires_at,
            effect,
        ) {
            self.unavailable = true;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modal_contract::ClientId;
    #[derive(Default)]
    struct Fake {
        events: Vec<&'static str>,
        refuse_dismiss: bool,
    }
    impl Backend for Fake {
        fn present(&mut self, _: Fence, _: u64) -> Result<(), Error> {
            self.events.push("present");
            Ok(())
        }
        fn dismiss(&mut self, _: Fence) -> Result<Dismissal, Error> {
            self.events.push("dismiss");
            Ok(Dismissal {
                presenter_exited: !self.refuse_dismiss,
                compositor_dismissed: true,
            })
        }
        fn dispatch(&mut self, _: Fence, _: u64, _: Effect) -> Result<(), Error> {
            self.events.push("effect");
            Ok(())
        }
    }
    fn owner(id: u8) -> Owner {
        Owner {
            client: ClientId([id; 16]),
            client_epoch: NonZeroU64::new(1).unwrap(),
        }
    }
    fn key(owner: Owner, id: u64) -> RequestKey {
        RequestKey {
            owner,
            request_id: NonZeroU64::new(id).unwrap(),
        }
    }
    fn actor() -> Actor<Fake> {
        Actor::new(AuthorityEpoch([9; 16]), Fake::default(), 0).unwrap()
    }
    #[test]
    fn expiry_requires_revocation_before_successor_present() {
        let mut a = actor();
        let first = owner(1);
        let second = owner(2);
        a.connect(first, 0).unwrap();
        a.connect(second, 0).unwrap();
        let lease = a.acquire(key(first, 1), 10, 0).unwrap();
        a.acquire(key(second, 1), 10, 10).unwrap();
        assert_eq!(a.backend.events, ["present", "dismiss", "present"]);
        assert!(
            a.execute(key(first, 2), lease.fence, Effect::Reset, 11)
                .is_err()
        );
        assert_eq!(a.backend.events.len(), 3);
    }
    #[test]
    fn failed_revocation_freezes_all_successor_grants() {
        let mut a = actor();
        let first = owner(1);
        let second = owner(2);
        a.connect(first, 0).unwrap();
        a.connect(second, 0).unwrap();
        a.acquire(key(first, 1), 10, 0).unwrap();
        a.backend.refuse_dismiss = true;
        assert_eq!(
            a.acquire(key(second, 1), 10, 10),
            Err(Error::IncompleteDismissal)
        );
        assert_eq!(a.acquire(key(second, 2), 10, 11), Err(Error::Unavailable));
        assert_eq!(a.backend.events, ["present", "dismiss"]);
    }
    #[test]
    fn release_preserves_connection_and_replay_has_no_effect() {
        let mut a = actor();
        let first = owner(1);
        a.connect(first, 0).unwrap();
        let lease = a.acquire(key(first, 1), 20, 0).unwrap();
        a.execute(key(first, 2), lease.fence, Effect::EnterAsk, 1)
            .unwrap();
        assert!(
            a.execute(key(first, 2), lease.fence, Effect::Reset, 1)
                .is_err()
        );
        a.release(key(first, 3), lease.fence, 2).unwrap();
        a.acquire(key(first, 4), 20, 3).unwrap();
        assert_eq!(
            a.backend.events,
            ["present", "effect", "dismiss", "present"]
        );
    }
    #[test]
    fn stale_reconnect_does_not_dismiss_current_presenter() {
        let mut a = actor();
        let first = owner(1);
        a.connect(first, 0).unwrap();
        a.acquire(key(first, 1), 20, 0).unwrap();
        assert!(a.connect(first, 1).is_err());
        assert_eq!(a.backend.events, ["present"]);
    }
}
