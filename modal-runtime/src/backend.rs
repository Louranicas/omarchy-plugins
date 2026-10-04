//! Dedicated presenter integration; the compositor implementation is supplied by
//! the trusted native host. There is deliberately no success-shaped default.
use crate::actor::{Application, Backend, Dismissal, Effect, Error};
use crate::presenter::Presenter;
use modal_contract::{Fence, Owner};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct PresenterIdentity {
    pub pid: u32,
    pub start_ticks: String,
    pub namespace: String,
}
pub struct PresenterSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
}
pub trait Compositor {
    fn namespace(&self, _fence: Fence) -> Option<String> {
        None
    }
    fn prepare(&mut self, _fence: Fence) -> Result<Vec<(OsString, OsString)>, Error> {
        Ok(Vec::new())
    }
    fn move_workspace(
        &mut self,
        _fence: Fence,
        _target: &crate::targets::NativeTarget,
        _expected: &crate::workspace_move::MoveWorkspaceRequest,
        _deadline: Instant,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn maximize(
        &mut self,
        _fence: Fence,
        _target: &crate::targets::NativeTarget,
        _expected: &crate::vimarchy_action::MaximizeRequest,
        _policy: &crate::vimarchy_policy::Policy,
        _deadline: Instant,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn focus(
        &mut self,
        _fence: Fence,
        _target: &crate::targets::NativeTarget,
        _deadline: Instant,
    ) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    /// Observe mapped readiness of this dedicated PID/fence; command exit is insufficient.
    fn ready(&mut self, pid: u32, fence: Fence, deadline: Instant) -> Result<bool, Error>;
    /// Confirm only this fence's surfaces/submap are gone; do not reset unrelated state.
    fn dismissed(&mut self, fence: Fence, deadline: Instant) -> Result<bool, Error>;
    /// Synchronous effect submission/readback under this fence and absolute expiry.
    fn dispatch(&mut self, fence: Fence, effect: Effect, deadline: Instant) -> Result<(), Error>;
}
pub struct PresenterBackend<C> {
    compositor: C,
    specs: [PresenterSpec; 3],
    vimarchy_policy: Option<crate::vimarchy_policy::Policy>,
    vimarchy_policy_path: Option<PathBuf>,
    workspace_move_config: Option<(PathBuf, [u8; 32])>,
    identities: Vec<(Owner, Application)>,
    active: Option<(Fence, Presenter)>,
    pinned_programs: Option<[std::fs::File; 3]>,
    controller_endpoints: Option<[PathBuf; 3]>,
    controllers: Vec<(Owner, crate::process_identity::Pinned)>,
    presenter_process: Option<crate::process_identity::Pinned>,
    origin: Option<Instant>,
    suspend: Option<crate::suspend::SuspendGuard>,
}
impl<C: Compositor> PresenterBackend<C> {
    fn presenter_alive(&mut self) -> Result<bool, Error> {
        let alive = self
            .active
            .as_mut()
            .ok_or(Error::Unavailable)?
            .1
            .alive()
            .map_err(|_| Error::Unavailable)?;
        if alive {
            self.presenter_process
                .as_ref()
                .ok_or(Error::Unavailable)?
                .check()
                .map_err(|_| Error::Unavailable)?;
        }
        Ok(alive)
    }
    fn continuity(&mut self) -> Result<(), Error> {
        self.suspend
            .as_mut()
            .ok_or(Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)
    }
    /// Only trusted startup code may install the admission source. Each move
    /// reloads the bounded no-follow config and requires the explicit flag.
    pub fn with_workspace_move_config(mut self, path: Option<(PathBuf, [u8; 32])>) -> Self {
        self.workspace_move_config = path;
        self
    }
    pub fn with_vimarchy_policy(mut self, policy: Option<crate::vimarchy_policy::Policy>) -> Self {
        self.vimarchy_policy = policy;
        self
    }
    /// Configured hosts retain the trusted source and revalidate each effect.
    pub fn with_vimarchy_policy_path(mut self, path: Option<PathBuf>) -> Self {
        self.vimarchy_policy_path = path;
        self
    }
    pub fn with_pinned_programs(mut self, programs: [std::fs::File; 3]) -> Self {
        self.pinned_programs = Some(programs);
        self
    }
    pub fn with_controller_endpoints(mut self, endpoints: [PathBuf; 3]) -> Self {
        self.controller_endpoints = Some(endpoints);
        self
    }
    pub fn new(compositor: C, specs: [PresenterSpec; 3]) -> Self {
        Self {
            compositor,
            specs,
            vimarchy_policy: None,
            vimarchy_policy_path: None,
            workspace_move_config: None,
            identities: Vec::new(),
            active: None,
            pinned_programs: None,
            controller_endpoints: None,
            controllers: Vec::new(),
            presenter_process: None,
            origin: None,
            suspend: crate::suspend::SuspendGuard::new().ok(),
        }
    }
}
impl<C: Compositor> Backend for PresenterBackend<C> {
    fn register_controller(&mut self, owner: Owner, pid: u32) -> Result<(), Error> {
        if self.controller_endpoints.is_none() {
            return Ok(());
        }
        let process =
            crate::process_identity::Pinned::capture(pid).map_err(|_| Error::Unavailable)?;
        self.controllers
            .retain(|(old, _)| old.client != owner.client);
        if self.controllers.len() >= 32 {
            return Err(Error::Unavailable);
        }
        self.controllers.push((owner, process));
        Ok(())
    }
    fn presenter_identity(&mut self, fence: Fence) -> Result<Option<PresenterIdentity>, Error> {
        if self.controller_endpoints.is_none() {
            return Ok(None);
        }
        self.continuity()?;
        let (active, child) = self.active.as_mut().ok_or(Error::Unavailable)?;
        if *active != fence || !child.alive().map_err(|_| Error::Unavailable)? {
            return Err(Error::Unavailable);
        }
        let process = self.presenter_process.as_ref().ok_or(Error::Unavailable)?;
        process.check().map_err(|_| Error::Unavailable)?;
        let identity = process.identity();
        let namespace = self.compositor.namespace(fence).ok_or(Error::Unavailable)?;
        Ok(Some(PresenterIdentity {
            pid: identity.pid,
            start_ticks: identity.start_ticks.to_string(),
            namespace,
        }))
    }
    fn set_clock_origin(&mut self, origin: Instant) {
        self.origin = Some(origin);
    }
    fn register(&mut self, owner: Owner, app: Application) -> Result<(), Error> {
        if let Some(slot) = self
            .identities
            .iter_mut()
            .find(|(old, _)| old.client == owner.client)
        {
            *slot = (owner, app);
        } else {
            if self.identities.len() >= 32 {
                return Err(Error::Unavailable);
            }
            self.identities.push((owner, app));
        }
        Ok(())
    }
    fn present(&mut self, fence: Fence, expires_at: u64) -> Result<(), Error> {
        self.continuity()?;
        let lease_deadline = self
            .origin
            .and_then(|origin| origin.checked_add(Duration::from_millis(expires_at)))
            .ok_or(Error::Unavailable)?;
        if Instant::now() >= lease_deadline {
            return Err(Error::Unavailable);
        }
        if self.active.is_some() {
            return Err(Error::Unavailable);
        }
        let (_, app) = self
            .identities
            .iter()
            .find(|(owner, _)| *owner == fence.owner)
            .ok_or(Error::Unavailable)?;
        let index = match app {
            Application::Vimarchy => 0,
            Application::Ask => 1,
            Application::Yoohoo => 2,
        };
        let spec = &self.specs[index];
        if spec
            .environment
            .iter()
            .any(|(key, _)| key.to_string_lossy().starts_with("OMARCHY_MODAL_"))
        {
            return Err(Error::Unavailable);
        }
        let mut environment = spec.environment.clone();
        environment.extend(self.compositor.prepare(fence)?);
        if let Some(endpoints) = &self.controller_endpoints {
            let (_, controller) = self
                .controllers
                .iter()
                .find(|(owner, _)| *owner == fence.owner)
                .ok_or(Error::Unavailable)?;
            controller.check().map_err(|_| Error::Unavailable)?;
            let identity = controller.identity();
            environment.extend([
                (
                    "OMARCHY_MODAL_CONTROLLER_SOCKET".into(),
                    endpoints[index].as_os_str().to_owned(),
                ),
                (
                    "OMARCHY_MODAL_CONTROLLER_PID".into(),
                    identity.pid.to_string().into(),
                ),
                (
                    "OMARCHY_MODAL_CONTROLLER_START_TICKS".into(),
                    identity.start_ticks.to_string().into(),
                ),
            ]);
        }
        let presenter = if let Some(programs) = &self.pinned_programs {
            Presenter::spawn_pinned(&programs[index], &spec.arguments, &environment)
        } else {
            Presenter::spawn(&spec.program, &spec.arguments, &environment)
        }
        .map_err(|_| Error::Unavailable)?;
        let pid = presenter.pid();
        self.active = Some((fence, presenter));
        self.presenter_process =
            Some(crate::process_identity::Pinned::capture(pid).map_err(|_| Error::Unavailable)?);
        if !self.compositor.ready(
            pid,
            fence,
            (Instant::now() + Duration::from_millis(500)).min(lease_deadline),
        )? {
            return Err(Error::Unavailable);
        }
        self.continuity()?;
        if Instant::now() >= lease_deadline
            || !self
                .active
                .as_mut()
                .ok_or(Error::Unavailable)?
                .1
                .alive()
                .map_err(|_| Error::Unavailable)?
        {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    fn dismiss(&mut self, fence: Fence) -> Result<Dismissal, Error> {
        if let Some((active, presenter)) = self.active.as_mut() {
            if *active != fence {
                return Err(Error::Unavailable);
            }
            presenter
                .revoke(Instant::now() + Duration::from_millis(500))
                .map_err(|_| Error::Unavailable)?;
            self.active = None;
            self.presenter_process = None;
        }
        let compositor_dismissed = self
            .compositor
            .dismissed(fence, Instant::now() + Duration::from_millis(500))?;
        Ok(Dismissal {
            presenter_exited: true,
            compositor_dismissed,
        })
    }
    fn move_workspace(
        &mut self,
        fence: Fence,
        expires_at: u64,
        target: &crate::targets::NativeTarget,
        expected: &crate::workspace_move::MoveWorkspaceRequest,
    ) -> Result<(), Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
            || !self
                .identities
                .iter()
                .any(|(owner, app)| *owner == fence.owner && *app == Application::Vimarchy)
        {
            return Err(Error::Unavailable);
        }
        let deadline = self
            .origin
            .and_then(|o| o.checked_add(Duration::from_millis(expires_at)))
            .ok_or(Error::Unavailable)?
            .min(Instant::now() + Duration::from_millis(500));
        if Instant::now() >= deadline || !self.presenter_alive()? {
            return Err(Error::Unavailable);
        }
        self.continuity()?;
        let (path, digest) = self
            .workspace_move_config
            .as_ref()
            .ok_or(Error::Unavailable)?;
        let (config, current) =
            crate::configured::Config::read_with_digest(path).map_err(|_| Error::Unavailable)?;
        if current != *digest || !config.vimarchy_numeric_workspace_moves || !expected.valid() {
            return Err(Error::Unavailable);
        }
        if Instant::now() >= deadline {
            return Err(Error::Unavailable);
        }

        self.compositor
            .move_workspace(fence, target, expected, deadline)?;
        self.continuity().map_err(|_| Error::EffectUnconfirmed)
    }
    fn maximize(
        &mut self,
        fence: Fence,
        expires_at: u64,
        target: &crate::targets::NativeTarget,
        expected: &crate::vimarchy_action::MaximizeRequest,
    ) -> Result<(), Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
            || !self
                .identities
                .iter()
                .any(|(owner, app)| *owner == fence.owner && *app == Application::Vimarchy)
        {
            return Err(Error::Unavailable);
        }
        let deadline = self
            .origin
            .and_then(|o| o.checked_add(Duration::from_millis(expires_at)))
            .ok_or(Error::Unavailable)?
            .min(Instant::now() + Duration::from_millis(500));
        if Instant::now() >= deadline || !self.presenter_alive()? {
            return Err(Error::Unavailable);
        }
        self.continuity()?;
        let policy = self.vimarchy_policy.as_ref().ok_or(Error::Unavailable)?;
        if !expected.valid() || expected.policy_digest != policy.digest() {
            return Err(Error::Unavailable);
        }
        if let Some(path) = &self.vimarchy_policy_path {
            let current =
                crate::vimarchy_policy::Policy::load(path).map_err(|_| Error::Unavailable)?;
            if current.digest() != policy.digest() {
                return Err(Error::Unavailable);
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Unavailable);
        }

        self.compositor
            .maximize(fence, target, expected, policy, deadline)?;
        self.continuity().map_err(|_| Error::EffectUnconfirmed)
    }
    fn focus(
        &mut self,
        fence: Fence,
        expires_at: u64,
        target: &crate::targets::NativeTarget,
    ) -> Result<(), Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
        {
            return Err(Error::Unavailable);
        }
        let deadline = self
            .origin
            .and_then(|origin| origin.checked_add(Duration::from_millis(expires_at)))
            .ok_or(Error::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(Error::Unavailable);
        }
        if !self.presenter_alive()? {
            return Err(Error::Unavailable);
        }
        self.continuity()?;
        self.compositor.focus(fence, target, deadline)?;
        self.continuity().map_err(|_| Error::EffectUnconfirmed)
    }
    fn dispatch(&mut self, fence: Fence, expires_at: u64, effect: Effect) -> Result<(), Error> {
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != fence)
        {
            return Err(Error::Unavailable);
        }
        let deadline = self
            .origin
            .and_then(|origin| origin.checked_add(Duration::from_millis(expires_at)))
            .ok_or(Error::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(Error::Unavailable);
        }
        if !self.presenter_alive()? {
            return Err(Error::Unavailable);
        }
        self.continuity()?;
        self.compositor.dispatch(fence, effect, deadline)?;
        self.continuity().map_err(|_| Error::EffectUnconfirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modal_contract::{AuthorityEpoch, ClientId};
    use std::num::NonZeroU64;
    struct Readback {
        ready: bool,
        dismiss: bool,
        effects: usize,
    }
    impl Compositor for Readback {
        fn namespace(&self, _: Fence) -> Option<String> {
            Some("omarchy-modal-0123456789abcdef0123456789abcdef".into())
        }
        fn ready(&mut self, _: u32, _: Fence, _: Instant) -> Result<bool, Error> {
            Ok(self.ready)
        }
        fn dismissed(&mut self, _: Fence, _: Instant) -> Result<bool, Error> {
            Ok(self.dismiss)
        }
        fn dispatch(&mut self, _: Fence, _: Effect, deadline: Instant) -> Result<(), Error> {
            assert!(Instant::now() < deadline);
            self.effects += 1;
            Ok(())
        }
    }
    fn spec() -> PresenterSpec {
        PresenterSpec {
            program: "/usr/bin/sleep".into(),
            arguments: vec!["30".into()],
            environment: vec![],
        }
    }
    fn fence() -> Fence {
        Fence {
            authority_epoch: AuthorityEpoch([1; 16]),
            generation: NonZeroU64::new(1).unwrap(),
            owner: Owner {
                client: ClientId([1; 16]),
                client_epoch: NonZeroU64::new(1).unwrap(),
            },
        }
    }
    #[test]
    fn controller_injection_and_post_ready_identity_are_owned_by_full_fence() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = root.path().join("modal.sock");
        let mut backend = PresenterBackend::new(
            Readback {
                ready: true,
                dismiss: true,
                effects: 0,
            },
            [spec(), spec(), spec()],
        )
        .with_controller_endpoints([endpoint.clone(), endpoint.clone(), endpoint.clone()]);
        backend.set_clock_origin(Instant::now());
        backend.register(fence().owner, Application::Ask).unwrap();
        backend
            .register_controller(fence().owner, std::process::id())
            .unwrap();
        assert!(backend.presenter_identity(fence()).is_err());
        backend.present(fence(), 5000).unwrap();
        let receipt = backend.presenter_identity(fence()).unwrap().unwrap();
        assert_eq!(receipt.pid, backend.active.as_ref().unwrap().1.pid());
        assert!(receipt.start_ticks.parse::<u64>().unwrap() > 0);
        let environment = std::fs::read(format!("/proc/{}/environ", receipt.pid)).unwrap();
        let entries: Vec<_> = environment.split(|b| *b == 0).collect();
        let pid = format!("OMARCHY_MODAL_CONTROLLER_PID={}", std::process::id());
        let socket = format!("OMARCHY_MODAL_CONTROLLER_SOCKET={}", endpoint.display());
        assert!(entries.contains(&pid.as_bytes()));
        assert!(entries.contains(&socket.as_bytes()));
        let mut wrong = fence();
        wrong.generation = NonZeroU64::new(2).unwrap();
        assert!(backend.presenter_identity(wrong).is_err());
        backend.dismiss(fence()).unwrap();
        assert!(backend.presenter_identity(fence()).is_err());
    }
    #[test]
    fn clock_loss_blocks_new_presenter_before_spawn() {
        let mut backend = PresenterBackend::new(
            Readback {
                ready: true,
                dismiss: true,
                effects: 0,
            },
            [spec(), spec(), spec()],
        );
        backend.set_clock_origin(Instant::now());
        backend.register(fence().owner, Application::Ask).unwrap();
        backend.suspend.as_mut().unwrap().invalidate_for_test();
        assert!(backend.present(fence(), 5000).is_err());
        assert!(backend.active.is_none());
        assert_eq!(backend.compositor.effects, 0);
    }
    #[test]
    fn real_presenter_exit_does_not_substitute_for_compositor_dismissal() {
        let fence = fence();
        let mut b = PresenterBackend::new(
            Readback {
                ready: true,
                dismiss: false,
                effects: 0,
            },
            [spec(), spec(), spec()],
        );
        b.set_clock_origin(Instant::now());
        b.register(fence.owner, Application::Ask).unwrap();
        b.present(fence, 5000).unwrap();
        b.dispatch(fence, 5000, Effect::EnterAsk).unwrap();
        let proof = b.dismiss(fence).unwrap();
        assert!(proof.presenter_exited);
        assert!(!proof.compositor_dismissed);
        assert_eq!(b.compositor.effects, 1);
        assert!(b.active.is_none());
    }
    #[test]
    fn failed_readiness_keeps_child_custody_and_expired_effect_is_refused() {
        let fence = fence();
        let mut b = PresenterBackend::new(
            Readback {
                ready: false,
                dismiss: true,
                effects: 0,
            },
            [spec(), spec(), spec()],
        );
        b.set_clock_origin(Instant::now());
        b.register(fence.owner, Application::Ask).unwrap();
        assert_eq!(b.present(fence, 5000), Err(Error::Unavailable));
        assert!(b.active.is_some());
        assert_eq!(
            b.dispatch(fence, 0, Effect::EnterAsk),
            Err(Error::Unavailable)
        );
        assert_eq!(b.compositor.effects, 0);
        assert!(b.dismiss(fence).unwrap().presenter_exited);
    }
}
