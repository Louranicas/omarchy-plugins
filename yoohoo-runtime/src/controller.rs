//! Explicit owner-controlled daemon lifecycle. No automatic modal acquisition.
use crate::{
    Runtime,
    reconnect::{Policy, Sources, Status as SourceStatus},
    service::Service,
};
use desktop_io::{Endpoint, ProcessIdentity};
use modal_runtime::socket::ControlSocket;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Startup {
    pub modal_root: PathBuf,
    pub native_root: PathBuf,
    pub native_instance: String,
    pub host_pid: u32,
    pub host_start_ticks: u64,
    pub native_pid: u32,
    pub native_start_ticks: u64,
}
impl Startup {
    fn host(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.host_pid,
            start_ticks: self.host_start_ticks,
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        if !self.modal_root.is_absolute()
            || !self.native_root.is_absolute()
            || self.native_instance.is_empty()
            || self.native_instance.len() > 128
            || self.host_pid == 0
            || self.host_pid > i32::MAX as u32
            || self.native_pid == 0
            || self.native_pid > i32::MAX as u32
            || self.host_start_ticks == 0
            || self.native_start_ticks == 0
        {
            return Err(invalid());
        }
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::other("yoohoo controller unavailable")
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Ready,
    Active,
    Faulted,
    Stopped,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Status,
    Open,
    Quit,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub instance: Option<[u8; 16]>,
    pub revision: u64,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u8,
    pub instance: [u8; 16],
    pub revision: u64,
    pub phase: Phase,
    pub accepted: bool,
    pub cleanup_confirmed: bool,
}
enum State {
    Idle(Box<Sources>),
    Active(Box<Service>),
    Faulted,
    Stopped,
}
pub struct Controller {
    state: State,
    host_guard: modal_client::data::ProcessGuard,
    config: Startup,
    data_root: PathBuf,
    policy: Policy,
    instance: [u8; 16],
    revision: u64,
    observed: Option<(Phase, u64)>,
    clean: bool,
}
impl Controller {
    pub fn new(config: Startup, data_root: &Path, policy: Policy) -> io::Result<Self> {
        config.validate()?;
        let host_guard = modal_client::data::ProcessGuard::pin(config.host())?;
        if !data_root.is_absolute() {
            return Err(invalid());
        }
        let endpoint = Endpoint::discover_identity(
            &config.native_root,
            &config.native_instance,
            ProcessIdentity {
                pid: config.native_pid,
                start_ticks: config.native_start_ticks,
            },
        )
        .map_err(|_| invalid())?;
        let sources = Sources::new(
            endpoint,
            Runtime::empty().map_err(|_| invalid())?,
            Instant::now(),
            policy,
        )
        .map_err(|_| invalid())?;
        let mut instance = [0; 16];
        let n = unsafe {
            libc::getrandom(
                instance.as_mut_ptr().cast(),
                instance.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if n != 16 || instance == [0; 16] {
            return Err(invalid());
        }
        Ok(Self {
            state: State::Idle(Box::new(sources)),
            host_guard,
            config,
            data_root: data_root.into(),
            policy,
            instance,
            revision: 1,
            observed: None,
            clean: false,
        })
    }
    fn phase(&self) -> Phase {
        match &self.state {
            State::Idle(source) => {
                if source.status() == SourceStatus::Ready {
                    Phase::Ready
                } else if source.status() == SourceStatus::Exhausted {
                    Phase::Faulted
                } else {
                    Phase::Waiting
                }
            }
            State::Active(_) => Phase::Active,
            State::Faulted => Phase::Faulted,
            State::Stopped => Phase::Stopped,
        }
    }
    fn advance(&mut self) -> io::Result<()> {
        self.revision = self.revision.checked_add(1).ok_or_else(invalid)?;
        Ok(())
    }
    pub fn status(&self) -> Reply {
        Reply {
            version: 1,
            instance: self.instance,
            revision: self.revision,
            phase: self.phase(),
            accepted: true,
            cleanup_confirmed: self.clean,
        }
    }
    pub fn tick(&mut self) -> io::Result<()> {
        if self.host_guard.check().is_err() {
            self.clean = false;
            self.state = State::Faulted;
        }
        let mut close = false;
        match &mut self.state {
            State::Idle(source) => {
                if source.step().is_err() || source.status() == SourceStatus::Exhausted {
                    self.state = State::Faulted;
                }
            }
            State::Active(service) => {
                if service.step().is_err() {
                    self.clean = false;
                    self.state = State::Faulted;
                } else {
                    close = service.is_closed();
                }
            }
            _ => {}
        }
        if close {
            let old = std::mem::replace(&mut self.state, State::Faulted);
            if let State::Active(service) = old {
                match service.into_sources(self.policy) {
                    Ok(source) => {
                        self.state = State::Idle(Box::new(source));
                        self.clean = true;
                    }
                    Err(_) => {
                        self.clean = false;
                    }
                }
            }
        }
        let source_revision = match &self.state {
            State::Idle(s) => s.view().revision,
            _ => 0,
        };
        let observed = (self.phase(), source_revision);
        if self.observed != Some(observed) {
            self.advance()?;
            self.observed = Some(observed);
        }
        Ok(())
    }
    pub fn command(&mut self, request: Request) -> io::Result<Reply> {
        if request.version != 1 {
            return Err(invalid());
        }
        if matches!(request.operation, Operation::Status) {
            return Ok(self.status());
        }
        if request.instance != Some(self.instance) || request.revision != self.revision {
            let mut reply = self.status();
            reply.accepted = false;
            return Ok(reply);
        }
        // Consume the control precondition even on refused execution. A replay
        // cannot become a future Open after a source/session transition.
        self.advance()?;
        let accepted = match request.operation {
            Operation::Open if self.phase() == Phase::Ready => {
                let old = std::mem::replace(&mut self.state, State::Faulted);
                let State::Idle(source) = old else {
                    return Err(invalid());
                };
                let attempt = (|| {
                    self.host_guard.check()?;
                    let (native, runtime, origin) = source.take_ready().map_err(|_| invalid())?;
                    let listener = Service::bind(&self.data_root)?;
                    let mut client =
                        modal_client::Client::connect(&self.config.modal_root, self.config.host())
                            .map_err(|_| invalid())?;
                    client
                        .acquire(Duration::from_secs(5))
                        .map_err(|_| invalid())?;
                    Service::attach(listener, client, native, runtime, origin)
                })();
                self.clean = false;
                match attempt {
                    Ok(service) => {
                        self.state = State::Active(Box::new(service));
                        true
                    }
                    Err(_) => false,
                }
            }
            Operation::Quit => {
                let old = std::mem::replace(&mut self.state, State::Faulted);
                match old {
                    State::Idle(_) | State::Stopped => {
                        self.state = State::Stopped;
                        true
                    }
                    State::Active(mut service) => {
                        service.close();
                        self.clean = service.cleanup_confirmed();
                        if self.clean {
                            self.state = State::Stopped;
                        }
                        self.clean
                    }
                    State::Faulted => false,
                }
            }
            _ => false,
        };
        self.observed = None;
        let mut reply = self.status();
        reply.accepted = accepted;
        Ok(reply)
    }
    /// One owner-UID request at a time, max4096-byte frame and50ms read/write
    /// deadline. Malformed peers lose only their connection, never gain Open.
    pub fn serve_step(&mut self, socket: &ControlSocket) -> io::Result<()> {
        self.tick()?;
        let mut peer = match socket.accept() {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        };
        let end = Instant::now() + Duration::from_millis(50);
        let bytes = match peer.read_frame(end) {
            Ok(b) if b.len() <= 4096 => b,
            _ => return Ok(()),
        };
        let request = match serde_json::from_slice::<Request>(&bytes) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        // Execution has its own bounded Host budgets. Reply budget starts after
        // execution; a lost reply cannot authorize retry of Open or an effect.
        let reply = self.command(request)?;
        let bytes = serde_json::to_vec(&reply)?;
        let sent = peer.write_frame(&bytes, Instant::now() + Duration::from_millis(50));
        if sent.is_ok() && self.phase() == Phase::Stopped {
            // The normal authenticated client drops after verifying the reply.
            // Keep our pinned process alive until that EOF, with a finite grace
            // budget for a vanished or noncooperating client; never replay Quit.
            let _ = peer.read_frame(Instant::now() + Duration::from_millis(250));
        }
        Ok(())
    }
}
