//! One app-owned data endpoint. The arbiter alone spawns/reaps the presenter and
//! authorizes native effects. This service accepts only that leased process.
use crate::{
    Context, Session,
    native::{BoundView, Native, Observation},
    presenter::{self, Auth, Command, Counter, Reply, ResultBody, Row},
};
use modal_client::data::ProcessGuard;
use modal_runtime::socket::{Connection, ControlSocket};
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    time::{Duration, Instant},
};
use vimarchy_core::gestures::{Effect, Event};
fn error() -> io::Error {
    io::Error::other("presenter session unavailable")
}
pub struct Service {
    listener: ControlSocket,
    peer: ProcessGuard,
    connection: Option<Connection>,
    auth: Auth,
    session: Session,
    policy: crate::policy::Policy,
    rows: Vec<Row>,
    native: Native,
    bound: BoundView,
    arbiter: modal_client::Client,
    origin: Instant,
    revision: Counter,
    request: u64,
    closed: bool,
    renewed: Instant,
}
impl Service {
    /// Bind with ControlSocket before requesting the modal lease. The configured
    /// endpoint is root/modal.sock; caller supplies the already granted client.
    pub fn attach(
        listener: ControlSocket,
        arbiter: modal_client::Client,
        native: Native,
        observation: Observation,
    ) -> io::Result<Self> {
        Self::attach_with_policy(
            listener,
            arbiter,
            native,
            observation,
            crate::policy::Policy::default(),
        )
    }
    pub fn attach_with_policy(
        listener: ControlSocket,
        mut arbiter: modal_client::Client,
        mut native: Native,
        observation: Observation,
        policy: crate::policy::Policy,
    ) -> io::Result<Self> {
        let lease = arbiter.lease().ok_or_else(error)?;
        let presenter = lease.presenter.ok_or_else(error)?;
        let peer = ProcessGuard::pin(presenter.process())?;
        let auth = Auth::new(lease.fence, presenter.namespace()).map_err(|_| error())?;
        let origin = Instant::now();
        let revision = Counter::new(observation.revision()).ok_or_else(error)?;
        let context = Context {
            presenter: presenter.nonce(),
            fence: lease.fence,
            source_revision: revision.get(),
        };
        let deadline = lease
            .deadline
            .checked_duration_since(origin)
            .ok_or_else(error)?
            .as_millis()
            .try_into()
            .map_err(|_| error())?;
        let (mut session, _) = Session::open(
            context,
            deadline,
            0,
            observation.monitors(),
            observation.clients(),
            &BTreeMap::new(),
            policy.tap_ms(),
        )
        .map_err(|_| error())?;
        let mut rows = Vec::new();
        for monitor in observation.monitors() {
            rows.push(Row::Output {
                name: monitor.name.clone(),
                origin: [monitor.x, monitor.y],
                size: [
                    monitor.width / monitor.scale,
                    monitor.height / monitor.scale,
                ],
            });
        }
        for window in &session.snapshot().windows {
            let hint = session
                .allocation()
                .visible
                .get(&window.hint_id)
                .ok_or_else(error)?
                .clone();
            rows.push(Row::Hint {
                hint,
                output: window.monitor.clone(),
                at: window.at,
                size: window.size,
                focused: window.focused,
            });
        }
        if rows.len() > presenter::MAX_ROWS || rows.iter().any(|r| r.validate().is_err()) {
            return Err(error());
        }
        let view = arbiter.targets().map_err(|_| error())?;
        let bound = native.bind(&observation, view).map_err(|_| error())?;
        let now = origin.elapsed().as_millis() as u64;
        let ready = session.host_ready(context, now).map_err(|_| error())?;
        let mut service = Self {
            listener,
            peer,
            connection: None,
            auth,
            session,
            policy,
            rows,
            native,
            bound,
            arbiter,
            origin,
            revision,
            request: 0,
            closed: false,
            renewed: origin,
        };
        service.effects(ready)?;
        Ok(service)
    }
    pub fn policy(&self) -> &crate::policy::Policy {
        &self.policy
    }
    pub fn bind(root: &Path) -> io::Result<ControlSocket> {
        ControlSocket::bind(root)
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    fn now(&self) -> u64 {
        self.origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
    fn effects(&mut self, batch: crate::Batch) -> io::Result<()> {
        let effects = self
            .session
            .consume_batch(batch, self.now())
            .map_err(|_| error())?;
        if effects.iter().any(|e| {
            matches!(
                e,
                Effect::Swap { .. } | Effect::HoldTarget { .. } | Effect::SelectSource(_)
            )
        }) {
            return Err(io::Error::other("gesture effect not implemented"));
        }
        // Resolve unsupported custom/master actions before any batch effect.
        let mut maximize = None;
        for effect in &effects {
            if let Effect::DoubleTap { target, alt } = effect {
                match self
                    .native
                    .plan_double_tap(&self.bound, target, *alt, &self.policy)
                    .map_err(|_| error())?
                {
                    crate::action_plan::Decision::Disabled => {}
                    crate::action_plan::Decision::Maximize(plan) => {
                        maximize = Some(plan.request(*alt, &self.policy))
                    }
                    _ => {
                        return Err(io::Error::other(
                            "custom or master gesture effect not implemented",
                        ));
                    }
                }
            }
        }
        // Workspace selection is a fresh proposal, never a target or dispatcher supplied by UI.
        for effect in &effects {
            if let Effect::MoveWorkspace {
                source,
                destination,
                follow,
            } = effect
            {
                let vimarchy_core::gestures::WorkspaceTarget::Number(number) = destination else {
                    return Err(io::Error::other("scratchpad movement not implemented"));
                };
                let request = self
                    .native
                    .plan_workspace_move(&self.bound, source, *number, *follow)
                    .map_err(|_| error())?;
                self.native
                    .move_workspace(&mut self.arbiter, &self.bound, source, &request)
                    .map_err(|_| error())?;
            }
        }
        // Complete the window operation before reset emits its own submap event.
        if let Some(request) = maximize.take() {
            let target = effects
                .iter()
                .find_map(|effect| {
                    if let Effect::DoubleTap { target, .. } = effect {
                        Some(target)
                    } else {
                        None
                    }
                })
                .ok_or_else(error)?;
            self.native
                .maximize(&mut self.arbiter, &self.bound, target, &request)
                .map_err(|_| error())?;
        }
        for effect in effects {
            match effect {
                Effect::EnterSelection { two_letters } => self
                    .arbiter
                    .execute(if two_letters {
                        modal_runtime::actor::Effect::EnterVimarchyDouble
                    } else {
                        modal_runtime::actor::Effect::EnterVimarchy
                    })
                    .map_err(|_| error())?,
                Effect::ResetOwnedSubmap => self
                    .arbiter
                    .execute(modal_runtime::actor::Effect::Reset)
                    .map_err(|_| error())?,
                Effect::Focus(target) => self
                    .native
                    .focus(&mut self.arbiter, &self.bound, &target)
                    .map_err(|_| error())?,
                Effect::Hide => self.closed = true,
                _ => {}
            }
        }
        Ok(())
    }
    /// One finite service turn. A false return means no accepted data peer yet.
    pub fn step(&mut self) -> io::Result<bool> {
        let result = self.step_inner();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn step_inner(&mut self) -> io::Result<bool> {
        if self.closed {
            return Ok(false);
        }
        self.peer.check()?;
        if self.renewed.elapsed() >= Duration::from_millis(350) {
            let lease = self
                .arbiter
                .renew(Duration::from_secs(5))
                .map_err(|_| error())?;
            let deadline = lease
                .deadline
                .checked_duration_since(self.origin)
                .ok_or_else(error)?
                .as_millis()
                .try_into()
                .map_err(|_| error())?;
            self.session
                .renew(self.session.context(), deadline, self.now())
                .map_err(|_| error())?;
            self.renewed = Instant::now();
        }
        self.native
            .reconcile_own_focus(&mut self.arbiter, &mut self.bound)
            .map_err(|_| error())?;
        let batch = self
            .session
            .input(self.session.context(), Event::Tick, self.now())
            .map_err(|_| error())?;
        self.effects(batch)?;
        if self.closed {
            self.close();
            return Ok(false);
        }
        if self.connection.is_none() {
            match self.listener.accept() {
                Ok(connection) => {
                    self.peer.check_peer(connection.uid, connection.pid)?;
                    self.connection = Some(connection);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        let deadline = self
            .arbiter
            .lease()
            .ok_or_else(error)?
            .deadline
            .min(Instant::now() + Duration::from_millis(250));
        let bytes = self
            .connection
            .as_mut()
            .ok_or_else(error)?
            .read_frame(deadline)?;
        self.peer.check()?;
        let request = presenter::decode(&bytes).map_err(|_| error())?;
        if request.auth != self.auth || request.request.get() <= self.request {
            return Err(error());
        }
        self.request = request.request.get();
        let result =
            match request.command {
                Command::Read { cursor, revision } => {
                    let mut rows = self.rows.clone();
                    if let vimarchy_core::gestures::Phase::Radial(target) = self.session.phase() {
                        let source = self
                            .session
                            .snapshot()
                            .windows
                            .iter()
                            .find(|w| w.hint_id == target.stable_id)
                            .ok_or_else(error)?;
                        rows.push(Row::Radial {
                            source_hint: target.hint.clone(),
                            current_workspace: source.workspace,
                        });
                    }
                    if (cursor > 0 && revision != Some(self.revision))
                        || revision.is_some_and(|r| r != self.revision)
                        || cursor > rows.len()
                    {
                        return Err(error());
                    }
                    let end = (cursor + presenter::PAGE_ROWS).min(rows.len());
                    ResultBody::Page {
                        rows: rows[cursor..end].to_vec(),
                        next: (end < rows.len()).then_some(end),
                        phase: self.session.phase().into(),
                    }
                }
                Command::Input { revision, event } => {
                    println!(
                        "vimarchy_presenter_input_received=true event={event:?} now_ms={}",
                        self.now()
                    );
                    if revision != self.revision {
                        return Err(error());
                    }
                    let mut event = event.event().map_err(|_| error())?;
                    // Legacy workspace-select exits before cancel/reset when the selected
                    // source already belongs to this workspace. Do not let the reducer's
                    // ordinary move transition close the radial for this verified no-op.
                    if let Event::Workspace { key, alt } = &event
                        && let Some(vimarchy_core::gestures::WorkspaceTarget::Number(number)) =
                            vimarchy_core::gestures::WorkspaceTarget::from_key(*key)
                        && let vimarchy_core::gestures::Phase::Radial(source) = self.session.phase()
                        && self.session.snapshot().windows.iter().any(|w| {
                            w.hint_id == source.stable_id && w.workspace == i64::from(number)
                        })
                    {
                        let request = self
                            .native
                            .plan_workspace_move(&self.bound, source, number, !alt)
                            .map_err(|_| error())?;
                        if request.source == request.destination {
                            event = Event::Tick; // Still validates current session fence and lease deadline.
                            println!("workspace_current_noop=true state=radial");
                        }
                    }
                    let batch = self
                        .session
                        .input(self.session.context(), event, self.now())
                        .map_err(|_| error())?;
                    self.effects(batch)?;
                    ResultBody::Applied {
                        phase: self.session.phase().into(),
                    }
                }
                Command::Close { revision } => {
                    if revision != self.revision {
                        return Err(error());
                    }
                    self.closed = true;
                    ResultBody::Closed
                }
            };
        let reply = Reply {
            version: 1,
            request: request.request,
            auth: self.auth.clone(),
            revision: self.revision,
            result,
        };
        let bytes = serde_json::to_vec(&reply).map_err(|_| error())?;
        if bytes.len() > 4096 {
            return Err(error());
        }
        self.peer.check()?;
        self.connection
            .as_mut()
            .ok_or_else(error)?
            .write_frame(&bytes, deadline)?;
        if self.closed {
            self.close();
        }
        Ok(true)
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.session.invalidate();
        self.connection = None;
        let _ = self.arbiter.release();
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.close();
    }
}
