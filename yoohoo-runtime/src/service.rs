//! Leased Yoohoo data service; the shared arbiter retains all native authority.
use crate::{
    Command, Runtime,
    native::Native,
    presenter::{self, Operation, Reply, ResultBody},
};
use modal_client::{
    Client, View as Targets,
    data::ProcessGuard,
    presenter::{Auth, Counter},
};
use modal_runtime::socket::{Connection, ControlSocket};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};
fn invalid() -> io::Error {
    io::Error::other("yoohoo session unavailable")
}
pub struct Service {
    listener: ControlSocket,
    connection: Option<Connection>,
    peer: ProcessGuard,
    auth: Auth,
    client: Client,
    targets: Targets,
    native: Option<Native>,
    runtime: Runtime,
    origin: Instant,
    renewed: Instant,
    revision: Counter,
    request: u64,
    closed: bool,
    faulted: bool,
    released: Option<bool>,
}
impl Service {
    pub fn bind(root: &Path) -> io::Result<ControlSocket> {
        ControlSocket::bind(root)
    }
    pub fn attach(
        listener: ControlSocket,
        mut client: Client,
        native: Native,
        mut runtime: Runtime,
        origin: Instant,
    ) -> io::Result<Self> {
        let lease = client.lease().ok_or_else(invalid)?;
        let presenter = lease.presenter.ok_or_else(invalid)?;
        let peer = ProcessGuard::pin(presenter.process())?;
        let auth = Auth::new(lease.fence, presenter.namespace()).map_err(io::Error::other)?;
        let targets = client.targets().map_err(|_| invalid())?;
        client
            .execute(modal_runtime::actor::Effect::EnterYoohoo)
            .map_err(|_| invalid())?;
        runtime
            .execute(
                Command::Open,
                origin.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            )
            .map_err(|_| invalid())?;
        let now = Instant::now();
        Ok(Self {
            listener,
            connection: None,
            peer,
            auth,
            client,
            targets,
            native: Some(native),
            runtime,
            origin,
            renewed: now,
            revision: Counter::new(1).unwrap(),
            request: 0,
            closed: false,
            faulted: false,
            released: None,
        })
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
    fn fresh(&mut self, deadline: Instant) -> io::Result<bool> {
        self.peer.check()?;
        let before = self.runtime.view().revision;
        let end = deadline.min(Instant::now() + Duration::from_millis(250));
        let mut drained = false;
        for _ in 0..32 {
            if Instant::now() >= end {
                return Err(invalid());
            }
            let now = self.now();
            if !self
                .native
                .as_mut()
                .ok_or_else(invalid)?
                .poll_before(&mut self.runtime, now, end)
                .map_err(|_| invalid())?
            {
                drained = true;
                break;
            }
        }
        if !drained || Instant::now() >= end {
            return Err(invalid());
        }
        let changed = before != self.runtime.view().revision;
        if changed {
            // New native state must never inherit old arbiter target tokens.
            Self::reserve(deadline, Duration::from_millis(500))?;
            self.targets = self.client.targets().map_err(|_| invalid())?;
            self.revision = Counter::new(self.revision.get().checked_add(1).ok_or_else(invalid)?)
                .ok_or_else(invalid)?;
        }
        Ok(changed)
    }
    fn reserve(deadline: Instant, budget: Duration) -> io::Result<()> {
        if deadline.saturating_duration_since(Instant::now()) < budget {
            Err(invalid())
        } else {
            Ok(())
        }
    }
    pub fn step(&mut self) -> io::Result<bool> {
        let result = self.step_inner();
        if result.is_err() {
            self.faulted = true;
            self.close();
        }
        result
    }
    fn step_inner(&mut self) -> io::Result<bool> {
        if self.closed {
            return Ok(false);
        }
        let step_deadline = self
            .client
            .lease()
            .ok_or_else(invalid)?
            .deadline
            .min(Instant::now() + Duration::from_secs(1));
        self.fresh(step_deadline)?;
        if self.renewed.elapsed() >= Duration::from_millis(350) {
            Self::reserve(step_deadline, Duration::from_millis(500))?;
            self.client
                .renew(Duration::from_secs(5))
                .map_err(|_| invalid())?;
            self.renewed = Instant::now();
        }
        if self.connection.is_none() {
            match self.listener.accept() {
                Ok(peer) => {
                    self.peer.check_peer(peer.uid, peer.pid)?;
                    self.connection = Some(peer)
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        let deadline = self
            .client
            .lease()
            .ok_or_else(invalid)?
            .deadline
            .min(Instant::now() + Duration::from_millis(250))
            .min(step_deadline);
        if !self.connection.as_mut().ok_or_else(invalid)?.read_ready()? {
            return Ok(false);
        }
        let bytes = self
            .connection
            .as_mut()
            .ok_or_else(invalid)?
            .read_frame(deadline)?;
        self.fresh(step_deadline)?;
        let request = presenter::decode(&bytes)?;
        if request.auth != self.auth || request.request.get() <= self.request {
            return Err(invalid());
        }
        self.request = request.request.get();
        let result = match request.command {
            Operation::Read { cursor, revision } => {
                if (cursor > 0 && revision != Some(self.revision))
                    || revision.is_some_and(|r| r != self.revision)
                {
                    ResultBody::Changed
                } else {
                    let mut view = self.runtime.view();
                    view.effect_backend =
                        "Shared modal focus; tag/audio/storage unavailable; settings volatile"
                            .into();
                    let (view, next, metadata_truncated) = presenter::page(view, cursor)?;
                    ResultBody::Page {
                        view: Box::new(view),
                        next,
                        metadata_truncated,
                    }
                }
            }
            Operation::Apply { revision, command } => {
                if revision != self.revision || matches!(command, Command::Open | Command::List) {
                    return Err(invalid());
                }
                let next = Counter::new(self.revision.get().checked_add(1).ok_or_else(invalid)?)
                    .ok_or_else(invalid)?;
                self.runtime
                    .execute(command, self.now())
                    .map_err(|_| invalid())?;
                if let Some(intent) = self.runtime.take_activation() {
                    if self.fresh(step_deadline)? {
                        return Err(invalid());
                    }
                    let (instance, stable) = self
                        .native
                        .as_ref()
                        .ok_or_else(invalid)?
                        .stable_target(&intent.key)
                        .ok_or_else(invalid)?;
                    let index = self
                        .targets
                        .rows()
                        .iter()
                        .position(|row| {
                            row.target.instance() == instance && row.target.stable_id() == stable
                        })
                        .ok_or_else(invalid)?;
                    Self::reserve(step_deadline, Duration::from_millis(500))?;
                    self.client
                        .focus(&self.targets, index)
                        .map_err(|_| invalid())?;
                    println!("yoohoo_activation_confirmed=true");
                    self.closed = true;
                }
                self.closed |= !self.runtime.view().open;
                self.revision = next;
                ResultBody::Applied {
                    closed: self.closed,
                }
            }
        };
        let reply = Reply {
            version: 1,
            request: request.request,
            auth: self.auth.clone(),
            revision: self.revision,
            result,
        };
        let bytes = serde_json::to_vec(&reply)?;
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        self.peer.check()?;
        self.connection
            .as_mut()
            .ok_or_else(invalid)?
            .write_frame(&bytes, deadline)?;
        if self.closed {
            self.close();
        }
        Ok(true)
    }
    /// Sticky observed release outcome. Closed alone never proves cleanup.
    pub fn cleanup_confirmed(&self) -> bool {
        !self.faulted && self.released == Some(true)
    }
    pub fn close(&mut self) {
        self.closed = true;
        let view = self.runtime.view();
        if view.open
            && self
                .runtime
                .execute(
                    Command::Close {
                        generation: view.generation,
                    },
                    self.now(),
                )
                .is_err()
        {
            self.faulted = true;
        }
        self.connection = None;
        if self.released.is_none() {
            self.released = Some(self.client.release().is_ok());
        }
    }
    /// Consumes old client/view/auth/data channel. Only source observations can
    /// resume after the Host confirms presenter reap and compositor dismissal.
    pub fn into_sources(
        mut self,
        policy: crate::reconnect::Policy,
    ) -> io::Result<crate::reconnect::Sources> {
        self.close();
        if !self.cleanup_confirmed() {
            return Err(invalid());
        }
        let native = self.native.take().ok_or_else(invalid)?;
        let runtime =
            std::mem::replace(&mut self.runtime, Runtime::empty().map_err(|_| invalid())?);
        crate::reconnect::Sources::resume(native, runtime, self.origin, policy)
            .map_err(|_| invalid())
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.close();
    }
}
