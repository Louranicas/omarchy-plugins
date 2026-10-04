//! Bounded single-threaded service host. Native backend calls must remain bounded.
use crate::actor::{Actor, Application, Backend, Effect};
use crate::protocol::{Action, decode};
use crate::socket::{Connection, ControlSocket, MAX_FRAME};
use modal_contract::{AuthorityEpoch, ClientId, Owner};
use serde_json::json;
use std::io;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CAPACITY: usize = 32;
struct Client {
    connection: Connection,
    owner: Owner,
    app: Application,
    session: [u8; 16],
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    deadline: Instant,
}
pub struct Host<B: Backend, P: FnMut(u32, i32) -> Option<Application>> {
    socket: ControlSocket,
    actor: Actor<B>,
    policy: P,
    origin: Instant,
    suspend: crate::suspend::SuspendGuard,
    clients: Vec<Option<Client>>,
    epochs: [u64; CAPACITY],
}
pub(crate) fn epoch() -> io::Result<AuthorityEpoch> {
    let mut bytes = [0; 16];
    let mut offset = 0;
    while offset < bytes.len() {
        // Kernel randomness supplies a new authority namespace; never reuse a fixed seed.
        let count = unsafe {
            libc::getrandom(
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
                libc::GRND_NONBLOCK,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::other("epoch unavailable"));
        }
        offset += count as usize;
    }
    Ok(AuthorityEpoch(bytes))
}
impl<B: Backend, P: FnMut(u32, i32) -> Option<Application>> Host<B, P> {
    pub fn bind(root: &Path, mut backend: B, policy: P) -> io::Result<Self> {
        let suspend = crate::suspend::SuspendGuard::new()?;
        let socket = ControlSocket::bind(root)?;
        let origin = Instant::now();
        backend.set_clock_origin(origin);
        let actor = Actor::new(epoch()?, backend, 0)
            .map_err(|_| io::Error::other("authority unavailable"))?;
        Ok(Self {
            socket,
            actor,
            policy,
            origin,
            suspend,
            clients: (0..CAPACITY).map(|_| None).collect(),
            epochs: [0; CAPACITY],
        })
    }
    pub fn begin_source(&mut self) -> Result<crate::targets::SourceTicket, crate::actor::Error> {
        self.actor.begin_source()
    }
    pub fn snapshot(
        &mut self,
        ticket: crate::targets::SourceTicket,
        sequence: u64,
        targets: Vec<crate::targets::NativeTarget>,
    ) -> Result<(), crate::actor::Error> {
        self.actor.snapshot(ticket, sequence, targets)
    }
    pub fn closed(
        &mut self,
        ticket: crate::targets::SourceTicket,
        sequence: u64,
        target: &crate::targets::NativeTarget,
    ) -> Result<(), crate::actor::Error> {
        self.actor.closed(ticket, sequence, target)
    }
    pub fn lost(
        &mut self,
        ticket: crate::targets::SourceTicket,
        sequence: u64,
    ) -> Result<(), crate::actor::Error> {
        self.actor.lost(ticket, sequence)
    }
    /// Process lease expiry even when native source synchronization is unavailable.
    pub fn maintenance(&mut self) -> Result<(), crate::actor::Error> {
        if self.suspend.check().is_err() {
            // Shutdown invalidates authority permanently, even if cleanup succeeds.
            if self.actor.shutdown().is_err() {
                self.socket.preserve_for_recovery();
            }
            return Err(crate::actor::Error::Unavailable);
        }
        self.actor.tick(self.now())
    }
    fn now(&self) -> u64 {
        self.origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
    fn admit(&mut self) {
        // Admission work is bounded even under a connection flood.
        for _ in 0..CAPACITY {
            let connection = match self.socket.accept() {
                Ok(c) => c,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => continue,
            };
            let Some(slot) = self.clients.iter().position(Option::is_none) else {
                continue;
            };
            let Some(app) = (self.policy)(connection.uid, connection.pid) else {
                continue;
            };
            let Some(next) = self.epochs[slot].checked_add(1).and_then(NonZeroU64::new) else {
                continue;
            };
            let mut id = [0; 16];
            id[0] = slot as u8 + 1;
            let owner = Owner {
                client: ClientId(id),
                client_epoch: next,
            };
            let Ok(session) = epoch() else {
                continue;
            };
            self.epochs[slot] = next.get();
            if self
                .actor
                .connect_application(owner, app, self.now())
                .is_err()
            {
                continue;
            }
            if self
                .actor
                .register_controller(owner, connection.pid as u32)
                .is_err()
            {
                let _ = self.actor.disconnect(owner, self.now());
                continue;
            }
            self.clients[slot] = Some(Client {
                connection,
                owner,
                app,
                session: session.0,
                input: Vec::new(),
                output: Vec::new(),
                offset: 0,
                deadline: Instant::now() + Duration::from_secs(1),
            });
        }
    }
    fn request(&mut self, client: &Client, frame: &[u8]) -> Vec<u8> {
        let Ok((key, action)) = decode(frame, client.owner) else {
            return b"{\"error\":\"invalid_request\"}\n".to_vec();
        };
        if (self.policy)(client.connection.uid, client.connection.pid) != Some(client.app) {
            let _ = self.actor.disconnect(client.owner, self.now());
            return b"{\"error\":\"permission_denied\"}\n".to_vec();
        }
        if self.maintenance().is_err() {
            return b"{\"error\":\"unavailable\"}\n".to_vec();
        }
        let now = self.now();
        let result = match action {
            Action::ListTargets { fence, cursor } => {
                if !matches!(client.app, Application::Yoohoo | Application::Vimarchy) {
                    return b"{\"error\":\"permission_denied\"}\n".to_vec();
                }
                let reply = match self
                    .actor
                    .list_targets(key, fence, client.session, cursor, now)
                {
                    Ok(page) => {
                        json!({"version":1,"request_id":key.request_id.to_string(),"status":"targets","revision":page.revision.to_string(),"producer_session":page.producer_session,"next":page.next,"rows":page.rows.iter().map(|row|json!({"token":row.token.as_str(),"instance":row.target.instance(),"stable_id":row.target.stable_id()})).collect::<Vec<_>>()})
                    }
                    Err(error) => {
                        json!({"version":1,"request_id":key.request_id.to_string(),"error":format!("{error:?}")})
                    }
                };
                let mut bytes = serde_json::to_vec(&reply).expect("closed reply");
                if bytes.len() > MAX_FRAME {
                    return b"{\"error\":\"reply_limit\"}\n".to_vec();
                }
                bytes.push(b'\n');
                return bytes;
            }
            Action::MoveWorkspace {
                fence,
                intent,
                expected,
            } => {
                if client.app != Application::Vimarchy || intent.producer_session != client.session
                {
                    return b"{\"error\":\"permission_denied\"}\n".to_vec();
                }
                self.actor
                    .move_workspace(key, fence, &intent, &expected, now)
                    .map(|()| None)
            }
            Action::MaximizeWindow {
                fence,
                intent,
                expected,
            } => {
                if client.app != Application::Vimarchy || intent.producer_session != client.session
                {
                    return b"{\"error\":\"permission_denied\"}\n".to_vec();
                }
                self.actor
                    .maximize_window(key, fence, &intent, &expected, now)
                    .map(|()| None)
            }
            Action::FocusWindow { fence, intent } => {
                if !matches!(client.app, Application::Yoohoo | Application::Vimarchy)
                    || intent.producer_session != client.session
                {
                    return b"{\"error\":\"permission_denied\"}\n".to_vec();
                }
                self.actor
                    .focus_window(key, fence, &intent, now)
                    .map(|()| None)
            }

            Action::Acquire { ttl } => self.actor.acquire(key, ttl, now).map(Some),
            Action::Renew { fence, ttl } => self.actor.renew(key, fence, ttl, now).map(Some),
            Action::Release { fence } => self.actor.release(key, fence, now).map(|()| None),
            Action::Execute { fence, effect } => {
                let allowed = matches!(
                    (client.app, effect),
                    (_, Effect::Reset)
                        | (Application::Ask, Effect::EnterAsk)
                        | (Application::Vimarchy, Effect::EnterVimarchy)
                        | (Application::Vimarchy, Effect::EnterVimarchyDouble)
                        | (Application::Yoohoo, Effect::EnterYoohoo)
                );
                if !allowed {
                    return b"{\"error\":\"permission_denied\"}\n".to_vec();
                }
                self.actor.execute(key, fence, effect, now).map(|()| None)
            }
        };
        let reply = match result {
            Ok(Some(lease)) => {
                let presenter = match self.actor.presenter_identity(lease.fence) {
                    Ok(value) => value,
                    Err(_) => {
                        if self.actor.shutdown().is_err() {
                            self.socket.preserve_for_recovery();
                        }
                        return b"{\"error\":\"unavailable\"}\n".to_vec();
                    }
                };
                json!({"version":1,"request_id":key.request_id.to_string(),"status":"granted","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"client_id":lease.fence.owner.client.0,"client_epoch":lease.fence.owner.client_epoch.to_string(),"expires_ms":lease.expires_at.to_string(),"presenter":presenter})
            }
            Ok(None) => {
                json!({"version":1,"request_id":key.request_id.to_string(),"status":"complete"})
            }
            Err(error) => {
                json!({"version":1,"request_id":key.request_id.to_string(),"error":format!("{error:?}")})
            }
        };
        let mut bytes = serde_json::to_vec(&reply).expect("closed reply");
        bytes.push(b'\n');
        bytes
    }
    /// One bounded polling turn. Called repeatedly by run or an embedding service.
    pub fn step(&mut self) -> io::Result<()> {
        self.maintenance()
            .map_err(|_| io::Error::other("modal authority unavailable"))?;
        self.admit();
        for slot in 0..CAPACITY {
            let Some(mut client) = self.clients[slot].take() else {
                continue;
            };
            let mut keep = Instant::now() < client.deadline
                && (self.policy)(client.connection.uid, client.connection.pid) == Some(client.app);
            if keep && !client.output.is_empty() {
                match client.connection.try_write(&client.output[client.offset..]) {
                    Ok(0) => keep = false,
                    Ok(n) => {
                        client.offset += n;
                        if client.offset == client.output.len() {
                            client.output.clear();
                            client.offset = 0;
                            client.deadline = Instant::now() + Duration::from_secs(1);
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => keep = false,
                }
            } else if keep {
                let mut buffer = [0; 512];
                let room = (MAX_FRAME + 1 - client.input.len()).min(buffer.len());
                match client.connection.try_read(&mut buffer[..room]) {
                    Ok(0) => keep = false,
                    Ok(n) => client.input.extend_from_slice(&buffer[..n]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => keep = false,
                }
                if keep && let Some(end) = client.input.iter().position(|byte| *byte == b'\n') {
                    client.output = self.request(&client, &client.input[..end]);
                    client.input.drain(..=end);
                    client.deadline = Instant::now() + Duration::from_millis(500);
                } else if client.input.len() > MAX_FRAME {
                    keep = false;
                }
            }
            if keep {
                self.clients[slot] = Some(client);
            } else {
                self.actor
                    .disconnect(client.owner, self.now())
                    .map_err(|_| io::Error::other("dismissal unconfirmed"))?;
            }
        }
        Ok(())
    }
    pub fn run(&mut self, cancelled: &AtomicBool) -> io::Result<()> {
        while !cancelled.load(Ordering::Acquire) {
            self.step()?;
            std::thread::sleep(Duration::from_millis(2));
        }
        self.actor
            .shutdown()
            .map_err(|_| io::Error::other("dismissal unconfirmed"))
    }
}
impl<B: Backend, P: FnMut(u32, i32) -> Option<Application>> Drop for Host<B, P> {
    fn drop(&mut self) {
        if !matches!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.actor.shutdown())),
            Ok(Ok(()))
        ) {
            self.socket.preserve_for_recovery();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{Dismissal, Error};
    use modal_contract::Fence;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    struct Fake {
        events: Arc<Mutex<Vec<&'static str>>>,
        refuse: bool,
    }
    impl Backend for Fake {
        fn focus(
            &mut self,
            _: Fence,
            _: u64,
            _: &crate::targets::NativeTarget,
        ) -> Result<(), Error> {
            self.events.lock().unwrap().push("focus");
            Ok(())
        }

        fn register(&mut self, _: Owner, _: Application) -> Result<(), Error> {
            Ok(())
        }
        fn present(&mut self, _: Fence, _: u64) -> Result<(), Error> {
            self.events.lock().unwrap().push("present");
            Ok(())
        }
        fn dismiss(&mut self, _: Fence) -> Result<Dismissal, Error> {
            self.events.lock().unwrap().push("dismiss");
            Ok(Dismissal {
                presenter_exited: !self.refuse,
                compositor_dismissed: !self.refuse,
            })
        }
        fn dispatch(&mut self, _: Fence, _: u64, _: Effect) -> Result<(), Error> {
            self.events.lock().unwrap().push("effect");
            Ok(())
        }
    }
    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }
    fn exchange<B: Backend, P: FnMut(u32, i32) -> Option<Application>>(
        host: &mut Host<B, P>,
        client: &mut UnixStream,
        value: serde_json::Value,
    ) -> serde_json::Value {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        client.write_all(&bytes).unwrap();
        for _ in 0..8 {
            host.step().unwrap();
        }
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    #[test]
    fn changed_controller_registration_revokes_existing_connection() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(AtomicBool::new(true));
        let policy = allowed.clone();
        let mut host = Host::bind(
            root.path(),
            Fake {
                events: events.clone(),
                refuse: false,
            },
            move |_, _| policy.load(Ordering::Acquire).then_some(Application::Ask),
        )
        .unwrap();
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let reply = exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        assert_eq!(reply["status"], "granted");
        allowed.store(false, Ordering::Release);
        host.step().unwrap();
        assert!(host.clients.iter().all(Option::is_none));
        assert_eq!(&*events.lock().unwrap(), &["present", "dismiss"]);
    }
    #[test]
    fn clock_loss_revokes_active_authority_and_cannot_resume() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events: events.clone(),
                refuse: false,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let reply = exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        assert_eq!(reply["status"], "granted");
        host.suspend.invalidate_for_test();
        assert!(host.maintenance().is_err());
        assert_eq!(&*events.lock().unwrap(), &["present", "dismiss"]);
        assert!(host.step().is_err());
        assert_eq!(&*events.lock().unwrap(), &["present", "dismiss"]);
    }
    #[test]
    fn reconnect_mints_new_owner_epoch_and_rejects_old_fence() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events,
                refuse: false,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let mut first = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let old = exchange(
            &mut host,
            &mut first,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        let old_owner = host.clients[0].as_ref().unwrap().owner;
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(1);
        while host.clients[0].is_some() && Instant::now() < deadline {
            host.step().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut second = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let new = exchange(
            &mut host,
            &mut second,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        assert_eq!(new["status"], "granted");
        assert!(host.clients[0].as_ref().unwrap().owner.client_epoch > old_owner.client_epoch);
        let stale = exchange(
            &mut host,
            &mut second,
            json!({"version":1,"request_id":"2","command":{"op":"execute","epoch":old["epoch"],"generation":old["generation"],"effect":"reset"}}),
        );
        assert!(stale["error"].is_string());
    }

    #[test]
    fn idle_admission_capacity_recovers_without_new_authority() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events,
                refuse: false,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let clients: Vec<_> = (0..CAPACITY)
            .map(|_| UnixStream::connect(root.path().join("modal.sock")).unwrap())
            .collect();
        host.step().unwrap();
        assert_eq!(
            host.clients.iter().filter(|c| c.is_some()).count(),
            CAPACITY
        );
        for client in host.clients.iter_mut().flatten() {
            client.deadline = Instant::now();
        }
        host.step().unwrap();
        assert!(host.clients.iter().all(Option::is_none));
        drop(clients);
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        assert_eq!(
            exchange(
                &mut host,
                &mut client,
                json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}})
            )["status"],
            "granted"
        );
    }

    #[test]
    fn socket_host_binds_scope_rejects_replay_and_cleans_on_disconnect() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events: events.clone(),
                refuse: false,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let reply = exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        assert_eq!(reply["status"], "granted");
        let denied = exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"2","command":{"op":"execute","epoch":reply["epoch"],"generation":reply["generation"],"effect":"enter_yoohoo"}}),
        );
        assert_eq!(denied["error"], "permission_denied");
        let effect = json!({"version":1,"request_id":"3","command":{"op":"execute","epoch":reply["epoch"],"generation":reply["generation"],"effect":"enter_ask"}});
        assert_eq!(
            exchange(&mut host, &mut client, effect.clone())["status"],
            "complete"
        );
        assert!(exchange(&mut host, &mut client, effect)["error"].is_string());
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(1);
        while host.clients.iter().any(Option::is_some) && Instant::now() < deadline {
            host.step().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*events.lock().unwrap(), ["present", "effect", "dismiss"]);
    }
    #[test]
    fn failed_dismissal_leaves_recovery_marker_and_refuses_restart() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events,
                refuse: true,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut refused = false;
        while Instant::now() < deadline {
            if host.step().is_err() {
                refused = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(refused);
        drop(host);
        assert!(root.path().join("modal.sock").exists());
        assert!(ControlSocket::bind(root.path()).is_err());
    }
    #[test]
    fn authority_epochs_are_fresh_and_unknown_peers_are_not_registered() {
        assert_ne!(epoch().unwrap(), epoch().unwrap());
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events: events.clone(),
                refuse: false,
            },
            |_, _| None,
        )
        .unwrap();
        let _client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        host.step().unwrap();
        assert!(host.clients.iter().all(Option::is_none));
        assert!(events.lock().unwrap().is_empty());
    }
    #[test]
    fn wire_focus_requires_issued_view_and_never_replays_closed_target() {
        for app in [Application::Yoohoo, Application::Vimarchy] {
            let root = root();
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut host = Host::bind(
                root.path(),
                Fake {
                    events: events.clone(),
                    refuse: false,
                },
                |_, _| Some(app),
            )
            .unwrap();
            let source = host.begin_source().unwrap();
            let target = crate::targets::NativeTarget::new("instance", "18000000").unwrap();
            host.snapshot(source, 1, vec![target.clone()]).unwrap();
            let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
            let lease = exchange(
                &mut host,
                &mut client,
                json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
            );
            let page = exchange(
                &mut host,
                &mut client,
                json!({"version":1,"request_id":"2","command":{"op":"list_targets","epoch":lease["epoch"],"generation":lease["generation"],"cursor":0}}),
            );
            assert_eq!(page["status"], "targets");
            let command = json!({"op":"focus_window","epoch":lease["epoch"],"generation":lease["generation"],"token":page["rows"][0]["token"],"displayed_revision":page["revision"],"producer_session":page["producer_session"]});
            let mut wrong = command.clone();
            wrong["producer_session"] = json!(vec![0; 16]);
            assert_eq!(
                exchange(
                    &mut host,
                    &mut client,
                    json!({"version":1,"request_id":"3","command":wrong})
                )["error"],
                "permission_denied"
            );
            assert_eq!(
                exchange(
                    &mut host,
                    &mut client,
                    json!({"version":1,"request_id":"4","command":command})
                )["status"],
                "complete"
            );
            assert!(
                exchange(
                    &mut host,
                    &mut client,
                    json!({"version":1,"request_id":"4","command":command})
                )["error"]
                    .is_string()
            );
            host.closed(source, 2, &target).unwrap();
            assert!(
                exchange(
                    &mut host,
                    &mut client,
                    json!({"version":1,"request_id":"5","command":command})
                )["error"]
                    .is_string()
            );
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| **e == "focus")
                    .count(),
                1
            );
        }
    }
    #[test]
    fn ask_has_no_window_registry_or_focus_capability() {
        let root = root();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::bind(
            root.path(),
            Fake {
                events: events.clone(),
                refuse: false,
            },
            |_, _| Some(Application::Ask),
        )
        .unwrap();
        let mut client = UnixStream::connect(root.path().join("modal.sock")).unwrap();
        let lease = exchange(
            &mut host,
            &mut client,
            json!({"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":5000}}),
        );
        for (id, command) in [
            (
                "2",
                json!({"op":"list_targets","epoch":lease["epoch"],"generation":lease["generation"],"cursor":0}),
            ),
            (
                "3",
                json!({"op":"focus_window","epoch":lease["epoch"],"generation":lease["generation"],"token":"0".repeat(48),"displayed_revision":"1","producer_session":vec![0;16]}),
            ),
        ] {
            assert_eq!(
                exchange(
                    &mut host,
                    &mut client,
                    json!({"version":1,"request_id":id,"command":command})
                )["error"],
                "permission_denied"
            );
        }
        assert!(!events.lock().unwrap().contains(&"focus"));
    }
}
