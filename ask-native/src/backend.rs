use crate::{Latest, Permission};
use ask_core::launch::FrozenLaunch;
use ask_core::session::{Phase, Role};
use ask_core::{Id, Text};
use ask_runtime::{Event, Worker};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub enum Command {
    Submit {
        sequence: u64,
        text: Text,
    },
    Answer {
        sequence: u64,
        permission: Permission,
        allow: bool,
    },
    Cancel {
        sequence: u64,
    },
    Pin {
        sequence: u64,
    },
    Focus(bool),
}
#[derive(Clone, Debug)]
pub struct Ack {
    pub sequence: u64,
    pub accepted: bool,
}
#[derive(Clone, Debug)]
pub struct View {
    pub revision: u64,
    pub ready: bool,
    pub busy: bool,
    pub pinned: bool,
    pub transcript: Vec<(Role, Text)>,
    pub permissions: Vec<Permission>,
    pub status: String,
    pub ack: Option<Ack>,
    pub attention: bool,
}
pub struct Handle {
    pub commands: SyncSender<Command>,
    pub views: Latest<View>,
    stop: Arc<AtomicBool>,
    admission: Arc<AtomicBool>,
    join: Option<JoinHandle<bool>>,
}
impl Handle {
    pub fn start(launch: FrozenLaunch) -> Self {
        Self::start_guarded(launch, None)
    }
    pub fn start_guarded(
        launch: FrozenLaunch,
        parent: Option<modal_runtime::readiness::ParentWatch>,
    ) -> Self {
        Self::start_with_environment_guarded(launch, vec![], parent)
    }
    pub fn start_with_environment_guarded(
        launch: FrozenLaunch,
        environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        parent: Option<modal_runtime::readiness::ParentWatch>,
    ) -> Self {
        Self::start_inner(launch, environment, parent, None)
    }
    /// Trusted UI calls admission only after its matching authenticated active ACK.
    /// The supplied preflight must describe this exact frozen launch.
    pub fn start_pending_guarded(
        launch: FrozenLaunch,
        environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        parent: modal_runtime::readiness::ParentWatch,
        preflight: ask_runtime::provider::Preflight,
    ) -> Self {
        Self::start_inner(launch, environment, Some(parent), Some(preflight))
    }
    fn start_inner(
        launch: FrozenLaunch,
        environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        parent: Option<modal_runtime::readiness::ParentWatch>,
        preflight: Option<ask_runtime::provider::Preflight>,
    ) -> Self {
        let admission = Arc::new(AtomicBool::new(preflight.is_none()));
        let pending = LaunchEnvironment {
            values: environment,
            preflight,
            admission: admission.clone(),
        };
        let (tx, rx) = mpsc::sync_channel(16);
        let views = Latest::default();
        let output = views.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let join = thread::spawn(move || run(launch, pending, rx, output, ending, parent));
        Self {
            commands: tx,
            views,
            stop,
            admission,
            join: Some(join),
        }
    }
    pub fn admit_after_managed_grant(&self) -> bool {
        if self.stop.load(Ordering::Acquire)
            || self.join.as_ref().is_none_or(|join| join.is_finished())
        {
            return false;
        }
        self.admission.store(true, Ordering::Release);
        true
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release)
    }
    /// Call after GTK quits. Worker operations are bounded, except kernel spawn; do
    /// not claim hard real-time cleanup if the OS itself cannot make progress.
    pub fn finish(mut self) -> bool {
        self.stop();
        self.join.take().is_some_and(|j| j.join().unwrap_or(false))
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.stop()
    }
}
struct LaunchEnvironment {
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    preflight: Option<ask_runtime::provider::Preflight>,
    admission: Arc<AtomicBool>,
}
fn run(
    launch: FrozenLaunch,
    environment: LaunchEnvironment,
    commands: Receiver<Command>,
    views: Latest<View>,
    stop: Arc<AtomicBool>,
    parent: Option<modal_runtime::readiness::ParentWatch>,
) -> bool {
    let parent_alive = || parent.as_ref().is_none_or(|watch| watch.check().is_ok());
    let admission_deadline = Instant::now() + Duration::from_secs(5);
    while !environment.admission.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) || !parent_alive() || Instant::now() >= admission_deadline {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    if stop.load(Ordering::Acquire) || !parent_alive() || Instant::now() >= admission_deadline {
        return true;
    }
    if environment
        .preflight
        .as_ref()
        .is_some_and(|checked| checked.recheck().is_err())
    {
        views.publish(View {
            revision: 1,
            ready: false,
            busy: false,
            pinned: false,
            transcript: vec![],
            permissions: vec![],
            status: "Provider paths changed before launch".into(),
            ack: None,
            attention: false,
        });
        return false;
    }
    if stop.load(Ordering::Acquire) || !parent_alive() || Instant::now() >= admission_deadline {
        return true;
    }
    let origin = Instant::now();
    let now = || origin.elapsed().as_millis() as u64;
    let mut worker = match Worker::spawn(launch, environment.values, 0) {
        Ok(w) => w,
        Err(e) => {
            views.publish(View {
                revision: 1,
                ready: false,
                busy: false,
                pinned: false,
                transcript: vec![],
                permissions: vec![],
                status: format!("Agent could not start: {e:?}"),
                ack: None,
                attention: false,
            });
            return false;
        }
    };
    let cancel = AtomicBool::new(false);
    let mut permissions = Vec::<Permission>::new();
    let mut revision = 0u64;
    let mut ack = None;
    let mut status = "Starting provider…".to_owned();
    let mut dirty = true;
    let mut terminal = false;
    let mut attention = false;
    'running: while !stop.load(Ordering::Acquire) {
        if !parent_alive() {
            println!("worker_parent_lost=true queued_actions_discarded=true");
            break;
        }
        for _ in 0..8 {
            if stop.load(Ordering::Acquire) || !parent_alive() {
                println!("worker_parent_lost_or_stopped=true queued_actions_discarded=true");
                break 'running;
            }
            let Ok(command) = commands.try_recv() else {
                break;
            };
            let is_submit = matches!(&command, Command::Submit { .. });
            let (sequence, result) = match command {
                Command::Submit { sequence, text } => {
                    let result = Id::new(format!("native-{sequence}"))
                        .map_err(ask_runtime::Error::Core)
                        .and_then(|turn| worker.adapter_mut().submit(turn, text, now()));
                    if result.is_ok() {
                        attention = false;
                    }
                    (Some(sequence), result)
                }
                Command::Answer {
                    sequence,
                    permission,
                    allow,
                } => {
                    let option = permission.choice(allow);
                    let result = worker.adapter_mut().answer(
                        permission.instance,
                        permission.generation,
                        permission.epoch,
                        &permission.request,
                        option.as_ref(),
                        now(),
                    );
                    if result.is_ok() {
                        permissions.retain(|p| p != &permission)
                    };
                    (Some(sequence), result)
                }
                Command::Cancel { sequence } => {
                    let result = worker.adapter_mut().cancel(now());
                    if result.is_ok() {
                        permissions.clear()
                    }
                    (Some(sequence), result)
                }
                Command::Pin { sequence } => (Some(sequence), worker.adapter_mut().pin()),
                Command::Focus(value) => {
                    worker.adapter_mut().set_focused(value);
                    if value && attention {
                        attention = false;
                        dirty = true;
                    }
                    (None, Ok(()))
                }
            };
            if let Some(sequence) = sequence {
                if is_submit {
                    ack = Some(Ack {
                        sequence,
                        accepted: result.is_ok(),
                    });
                }
                status = match result {
                    Ok(()) => "Working…".into(),
                    Err(e) => format!("Action rejected: {e:?}"),
                };
                dirty = true;
            }
        }
        if !parent_alive() {
            break;
        }
        match worker.pump(now(), &cancel) {
            Ok(events) => {
                for event in events {
                    dirty = true;
                    match event {
                        Event::Ready => status = "Ready · interactive approval".into(),
                        Event::Permission { .. } => {
                            if let Some(p) = Permission::from_event(event) {
                                if permissions.len() < 32 {
                                    permissions.push(p)
                                } else {
                                    status = "Permission queue limit reached".into();
                                    terminal = true;
                                }
                            }
                        }
                        Event::Fault(error) => {
                            status = format!("Agent disconnected: {error:?}");
                            permissions.clear();
                            terminal = true
                        }
                        Event::Attention => attention = true,
                        Event::Reaped => {
                            if !terminal {
                                status = "Agent stopped".into()
                            }
                            terminal = true
                        }
                        _ => {}
                    }
                }
            }
            Err(error) => {
                status = format!("Session error: {error:?}");
                dirty = true;
                terminal = true;
                permissions.clear()
            }
        }
        if worker.adapter().session().permissions().pending_count() == 0 && !permissions.is_empty()
        {
            permissions.clear();
            dirty = true
        }
        if dirty {
            let session = worker.adapter().session();
            let busy = session.active_turn().is_some();
            if !busy
                && !terminal
                && session.phase() == Phase::Ready
                && permissions.is_empty()
                && status == "Working…"
            {
                status = "Ready · interactive approval".into()
            }
            revision = match revision.checked_add(1) {
                Some(v) => v,
                None => break,
            };
            views.publish(View {
                revision,
                ready: session.phase() == Phase::Ready && !terminal,
                busy,
                pinned: session.presentation() == ask_core::session::Presentation::Pinned,
                transcript: session
                    .messages()
                    .iter()
                    .map(|m| (m.role, m.body.clone()))
                    .collect(),
                permissions: permissions.clone(),
                status: status.clone(),
                ack: ack.clone(),
                attention,
            });
            dirty = false;
        }
        thread::sleep(Duration::from_millis(8));
    }
    if !parent_alive() {
        // Revocation must discard unsent approvals, not flush them as part of a
        // graceful close. Already transmitted effects cannot be recalled.
        worker.adapter_mut().lost(now());
        cancel.store(true, Ordering::Release);
    } else {
        let _ = worker.adapter_mut().close(now());
    }
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        let _ = worker.pump(now(), &cancel);
        if worker.reap_receipt().state() == acp_transport::ReapState::Reaped {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
