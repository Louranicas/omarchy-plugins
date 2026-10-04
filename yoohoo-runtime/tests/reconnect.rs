//! Real source/Host/data IPC recovery; compositor data and presenter lifecycle are simulated.
use desktop_io::Endpoint;
use std::{
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use yoohoo_runtime::{
    Command, Runtime,
    reconnect::{Policy, Sources, Status},
};
struct Fixture {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    events: Arc<Mutex<Vec<UnixStream>>>,
    queries: Arc<AtomicUsize>,
    event_count: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(dir.path().join("hypr/test")).unwrap();
        let q = UnixListener::bind(dir.path().join("hypr/test/.socket.sock")).unwrap();
        let e = UnixListener::bind(dir.path().join("hypr/test/.socket2.sock")).unwrap();
        q.set_nonblocking(true).unwrap();
        e.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
        let queries = Arc::new(AtomicUsize::new(0));
        let event_count = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicUsize::new(0));
        let (halt, streams, count, accepts, fault) = (
            stop.clone(),
            events.clone(),
            queries.clone(),
            event_count.clone(),
            mode.clone(),
        );
        let worker = thread::spawn(move || {
            while !halt.load(Ordering::Acquire) {
                if let Ok((stream, _)) = e.accept() {
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    streams.lock().unwrap().push(stream);
                    accepts.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok((mut peer, _)) = q.accept() {
                    peer.set_read_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    peer.set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    let mut bytes = [0; 128];
                    if let Ok(n) = peer.read(&mut bytes) {
                        count.fetch_add(1, Ordering::Relaxed);
                        let clients = &bytes[..n] == b"j/clients";
                        let mode = fault.load(Ordering::Acquire);
                        if mode == 1 && clients {
                            for stream in streams.lock().unwrap().iter_mut() {
                                let _ = stream.write_all(b"urgent>>0x");
                            }
                        }
                        if mode == 2 {
                            thread::sleep(Duration::from_millis(80));
                        }
                        let reply = if clients {
                            r#"[{"address":"0x1","stableId":"a","title":"Live","class":"Terminal","workspace":{"id":1,"name":"One"}}]"#
                        } else {
                            "{}"
                        };
                        let _ = peer.write_all(reply.as_bytes());
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            dir,
            stop,
            worker: Some(worker),
            events,
            queries,
            event_count,
            mode,
        }
    }
    fn sources(&self, failures: u8) -> Sources {
        let endpoint = Endpoint::discover(self.dir.path(), "test", std::process::id()).unwrap();
        Sources::new(
            endpoint,
            Runtime::empty().unwrap(),
            Instant::now(),
            Policy {
                initial: Duration::from_millis(30),
                maximum: Duration::from_millis(60),
                jitter: Duration::ZERO,
                attempt: Duration::from_millis(100),
                failures,
            },
        )
        .unwrap()
    }
    fn edge(&self, bytes: &[u8]) {
        let end = Instant::now() + Duration::from_secs(1);
        loop {
            let mut streams = self.events.lock().unwrap();
            if let Some(last) = streams.last_mut() {
                last.write_all(bytes).unwrap();
                return;
            }
            drop(streams);
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn disconnect(&self) {
        for peer in self.events.lock().unwrap().drain(..) {
            peer.shutdown(std::net::Shutdown::Both).unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}
fn wait(s: &mut Sources, predicate: impl Fn(Status) -> bool) -> Status {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let state = s.step().unwrap();
        if predicate(state) {
            return state;
        }
        assert!(Instant::now() < end, "{state:?}");
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn socket_loss_resnapshots_without_reusing_old_attention_or_selection() {
    let fixture = Fixture::new();
    let mut source = fixture.sources(3);
    assert_eq!(source.step().unwrap(), Status::Ready);
    fixture.edge(b"urgent>>0x1\n");
    wait(&mut source, |_| true);
    let old = source.view();
    assert_eq!(old.rows.len(), 1);
    assert!(!old.open);
    fixture.disconnect();
    assert!(matches!(
        source.step().unwrap(),
        Status::Waiting { failures: 1, .. }
    ));
    assert!(source.view().stale);
    assert!(!source.view().open);
    let before = fixture.event_count.load(Ordering::Relaxed);
    assert!(matches!(source.step().unwrap(), Status::Waiting { .. }));
    assert_eq!(before, fixture.event_count.load(Ordering::Relaxed));
    wait(&mut source, |s| s == Status::Ready);
    assert!(
        source.view().rows.is_empty(),
        "old alert resurrected after fresh epoch"
    );
    assert!(!source.view().stale);
    fixture.edge(b"urgent>>0x1\n");
    wait(&mut source, |_| true);
    let fresh = source.view();
    assert_eq!(fresh.rows.len(), 1);
    assert_ne!(old.rows[0].id, fresh.rows[0].id);
    let (_native, mut runtime, origin) = source.take_ready().unwrap();
    assert!(!runtime.view().open);
    assert!(runtime.take_activation().is_none());
    let now = origin.elapsed().as_millis() as u64;
    runtime.execute(Command::Open, now).unwrap();
    assert!(
        runtime
            .execute(
                Command::Activate {
                    generation: old.generation,
                    revision: old.revision,
                    id: old.rows[0].id
                },
                now
            )
            .is_err()
    );
    assert!(runtime.take_activation().is_none());
}
#[test]
fn partial_snapshot_race_exhausts_and_never_reports_ready() {
    let fixture = Fixture::new();
    fixture.mode.store(1, Ordering::Release);
    let mut source = fixture.sources(2);
    assert!(matches!(
        source.step().unwrap(),
        Status::Waiting { failures: 1, .. }
    ));
    assert!(source.view().stale);
    assert!(!source.view().open);
    wait(&mut source, |s| s == Status::Exhausted);
    let attempts = fixture.event_count.load(Ordering::Relaxed);
    assert_eq!(attempts, 2);
    for _ in 0..20 {
        assert_eq!(source.step().unwrap(), Status::Exhausted);
    }
    assert_eq!(attempts, fixture.event_count.load(Ordering::Relaxed));
}
#[test]
fn cumulative_attempt_deadline_is_not_renewed_for_second_query() {
    let fixture = Fixture::new();
    fixture.mode.store(2, Ordering::Release);
    let mut source = fixture.sources(1);
    let started = Instant::now();
    assert_eq!(source.step().unwrap(), Status::Exhausted);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "second query renewed budget"
    );
    assert_eq!(fixture.queries.load(Ordering::Relaxed), 2);
    assert!(source.view().stale);
}
#[test]
fn renamed_discovery_path_does_not_select_replacement_instance_directory() {
    let fixture = Fixture::new();
    let mut source = fixture.sources(2);
    assert_eq!(source.step().unwrap(), Status::Ready);
    fixture.disconnect();
    let old = fixture.dir.path().join("hypr/old");
    std::fs::rename(fixture.dir.path().join("hypr/test"), &old).unwrap();
    std::fs::remove_file(old.join(".socket2.sock")).unwrap();
    std::fs::create_dir(fixture.dir.path().join("hypr/test")).unwrap();
    let replacement =
        UnixListener::bind(fixture.dir.path().join("hypr/test/.socket2.sock")).unwrap();
    replacement.set_nonblocking(true).unwrap();
    assert!(matches!(
        source.step().unwrap(),
        Status::Waiting { failures: 1, .. }
    ));
    wait(&mut source, |s| s == Status::Exhausted);
    assert!(matches!(replacement.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    assert!(source.view().stale);
}

// A real Host protocol loop with explicit simulated lifecycle/readback only.
// Both source and modal/data paths use actual authenticated Unix sockets.
mod authority_join {
    use super::*;
    use modal_contract::{Fence, Owner};
    use modal_runtime::{
        actor::{Application, Backend, Dismissal, Effect, Error},
        backend::PresenterIdentity,
        host::Host,
        targets::NativeTarget,
    };
    use yoohoo_runtime::{
        native::Native,
        presenter::{Link, Model},
        service::Service,
    };
    fn identity() -> desktop_io::ProcessIdentity {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        desktop_io::ProcessIdentity {
            pid: std::process::id(),
            start_ticks: stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .parse()
                .unwrap(),
        }
    }
    struct Simulated {
        calls: Arc<AtomicUsize>,
        uncertain: Arc<AtomicBool>,
        bad_dismissal: Arc<AtomicBool>,
        grants: Arc<Mutex<Vec<Fence>>>,
    }
    impl Backend for Simulated {
        fn register(&mut self, _: Owner, app: Application) -> Result<(), Error> {
            assert_eq!(app, Application::Yoohoo);
            Ok(())
        }
        fn present(&mut self, _: Fence, _: u64) -> Result<(), Error> {
            Ok(())
        }
        fn presenter_identity(&mut self, fence: Fence) -> Result<Option<PresenterIdentity>, Error> {
            let id = identity();
            self.grants.lock().unwrap().push(fence);
            Ok(Some(PresenterIdentity {
                pid: id.pid,
                start_ticks: id.start_ticks.to_string(),
                namespace: format!("omarchy-modal-{:032x}", fence.generation.get()),
            }))
        }
        fn dismiss(&mut self, _: Fence) -> Result<Dismissal, Error> {
            Ok(Dismissal {
                presenter_exited: !self.bad_dismissal.load(Ordering::Acquire),
                compositor_dismissed: true,
            })
        }
        fn dispatch(&mut self, _: Fence, _: u64, effect: Effect) -> Result<(), Error> {
            assert_eq!(effect, Effect::EnterYoohoo);
            Ok(())
        }
        fn focus(&mut self, _: Fence, _: u64, target: &NativeTarget) -> Result<(), Error> {
            assert_eq!(target.stable_id(), "a");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.uncertain.load(Ordering::Acquire) {
                Err(Error::EffectUnconfirmed)
            } else {
                Ok(())
            }
        }
    }
    struct Thread {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }
    impl Thread {
        fn finish(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
        }
    }
    impl Drop for Thread {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
    fn host(fixture: &Fixture, calls: Arc<AtomicUsize>, uncertain: Arc<AtomicBool>) -> Thread {
        host_observed(
            fixture,
            calls,
            uncertain,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
        )
    }
    fn host_observed(
        fixture: &Fixture,
        calls: Arc<AtomicUsize>,
        uncertain: Arc<AtomicBool>,
        bad_dismissal: Arc<AtomicBool>,
        grants: Arc<Mutex<Vec<Fence>>>,
    ) -> Thread {
        host_for_peer(
            fixture,
            calls,
            uncertain,
            bad_dismissal,
            grants,
            Arc::new(AtomicUsize::new(std::process::id() as usize)),
        )
    }
    fn host_for_peer(
        fixture: &Fixture,
        calls: Arc<AtomicUsize>,
        uncertain: Arc<AtomicBool>,
        bad_dismissal: Arc<AtomicBool>,
        grants: Arc<Mutex<Vec<Fence>>>,
        allowed: Arc<AtomicUsize>,
    ) -> Thread {
        let root = fixture.dir.path().join("arbiter");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut host = Host::bind(
            &root,
            Simulated {
                calls,
                uncertain,
                bad_dismissal,
                grants,
            },
            move |_, pid| {
                (pid as usize == allowed.load(Ordering::Acquire)).then_some(Application::Yoohoo)
            },
        )
        .unwrap();
        let ticket = host.begin_source().unwrap();
        host.snapshot(ticket, 1, vec![NativeTarget::new("test", "a").unwrap()])
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let halted = stop.clone();
        let handle = thread::spawn(move || {
            while !halted.load(Ordering::Acquire) {
                if host.step().is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        Thread {
            stop,
            handle: Some(handle),
        }
    }
    fn service(
        fixture: &Fixture,
        client: modal_client::Client,
        parts: (Native, Runtime, Instant),
    ) -> Thread {
        let root = fixture.dir.path().join("data");
        if !root.exists() {
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (native, runtime, origin) = parts;
        let listener = Service::bind(&root).unwrap();
        let mut service = Service::attach(listener, client, native, runtime, origin).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let halted = stop.clone();
        let handle = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(3);
            while !halted.load(Ordering::Acquire) && !service.is_closed() && Instant::now() < end {
                if service.step().is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            service.close();
        });
        Thread {
            stop,
            handle: Some(handle),
        }
    }
    fn activation(model: &Model) -> Command {
        Command::Activate {
            generation: model.view.generation,
            revision: model.view.revision,
            id: model.view.rows[0].id,
        }
    }
    fn prepared(fixture: &Fixture) -> Sources {
        let mut source = fixture.sources(3);
        assert_eq!(source.step().unwrap(), Status::Ready);
        fixture.edge(b"urgent>>0x1\n");
        wait(&mut source, |_| true);
        assert_eq!(source.view().rows.len(), 1);
        source
    }
    fn run(mode: &str) {
        let fixture = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let uncertain = Arc::new(AtomicBool::new(false));
        let mut server = host(&fixture, calls.clone(), uncertain.clone());
        let first = prepared(&fixture).take_ready().unwrap();
        let mut client =
            modal_client::Client::connect(&fixture.dir.path().join("arbiter"), identity()).unwrap();
        let lease1 = client.acquire(Duration::from_secs(5)).unwrap();
        let old_targets = client.targets().unwrap();
        let auth1 =
            modal_client::presenter::Auth::new(lease1.fence, lease1.presenter.unwrap().namespace())
                .unwrap();
        let mut active = service(&fixture, client, first);
        let mut old_link = Link::connect(
            &fixture.dir.path().join("data/modal.sock"),
            identity(),
            auth1.clone(),
        )
        .unwrap();
        let old_model = old_link.model().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Observe loss through a real service request; then join before rebind.
        fixture.disconnect();
        assert!(old_link.model().is_err());
        active.finish();
        assert!(old_link.apply(activation(&old_model)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Explicit launcher-owned construction with the same trusted identity;
        // the new source itself also performs an EOF/backoff/resnapshot cycle.
        let mut recovered = prepared(&fixture);
        fixture.disconnect();
        assert!(matches!(recovered.step().unwrap(), Status::Waiting { .. }));
        wait(&mut recovered, |s| s == Status::Ready);
        assert!(recovered.view().rows.is_empty());
        fixture.edge(b"urgent>>0x1\n");
        wait(&mut recovered, |_| true);
        let parts = recovered.take_ready().unwrap();
        assert!(!parts.1.view().open);
        assert!(!parts.1.view().pending_activation);
        let mut fresh =
            modal_client::Client::connect(&fixture.dir.path().join("arbiter"), identity()).unwrap();
        let lease2 = fresh.acquire(Duration::from_secs(5)).unwrap();
        assert_ne!(lease1.fence, lease2.fence);
        assert_ne!(
            lease1.presenter.unwrap().namespace(),
            lease2.presenter.unwrap().namespace()
        );
        assert_eq!(
            fresh.focus(&old_targets, 0).unwrap_err(),
            modal_client::Error::Expired
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let auth2 =
            modal_client::presenter::Auth::new(lease2.fence, lease2.presenter.unwrap().namespace())
                .unwrap();
        let mut second = service(&fixture, fresh, parts);
        if matches!(mode, "old-auth" | "old-selection") {
            let mut wire = modal_client::data::Client::connect(
                &fixture.dir.path().join("data/modal.sock"),
                identity(),
            )
            .unwrap();
            let read = serde_json::json!({"version":1,"request":"1","auth":auth2,"command":{"op":"read","cursor":0,"revision":null}});
            let reply = wire
                .exchange(
                    &serde_json::to_vec(&read).unwrap(),
                    Instant::now() + Duration::from_millis(500),
                )
                .unwrap();
            let reply: yoohoo_runtime::presenter::Reply = serde_json::from_slice(&reply).unwrap();
            let yoohoo_runtime::presenter::ResultBody::Page { view, .. } = reply.result else {
                panic!("expected current model")
            };
            // Isolate each stale boundary: old auth with fully current command,
            // or current auth with the old captured selection. Both decode.
            let command = if mode == "old-auth" {
                Command::Activate {
                    generation: view.generation,
                    revision: view.revision,
                    id: view.rows[0].id,
                }
            } else {
                activation(&old_model)
            };
            let auth = if mode == "old-auth" { auth1 } else { auth2 };
            let request = serde_json::json!({"version":1,"request":"2","auth":auth,"command":{"op":"apply","revision":reply.revision,"command":command}});
            let bytes = serde_json::to_vec(&request).unwrap();
            yoohoo_runtime::presenter::decode(&bytes).unwrap();
            assert!(
                wire.exchange(&bytes, Instant::now() + Duration::from_millis(500))
                    .is_err(),
                "stale envelope or selection activated"
            );
            second.finish();
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        } else {
            let mut link = Link::connect(
                &fixture.dir.path().join("data/modal.sock"),
                identity(),
                auth2,
            )
            .unwrap();
            let model = link.model().unwrap();
            assert_eq!(model.view.rows.len(), 1);
            assert!(model.view.open);
            assert!(!model.view.pending_activation);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            if mode == "uncertain" {
                uncertain.store(true, Ordering::Release);
                assert!(
                    link.apply(activation(&model)).is_err(),
                    "uncertain effect was acknowledged"
                );
                assert!(link.apply(activation(&model)).is_err());
                second.finish();
                let again =
                    modal_client::Client::connect(&fixture.dir.path().join("arbiter"), identity());
                if let Ok(mut client) = again {
                    assert!(client.acquire(Duration::from_secs(1)).is_err());
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "uncertain native effect retried"
                );
            } else {
                assert!(link.apply(activation(&model)).unwrap());
                second.finish();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        }
        server.finish();
    }
    #[test]
    fn fresh_recovery_acquires_new_fence_and_only_fresh_view_activates() {
        run("positive");
    }
    #[test]
    fn replayed_old_presenter_envelope_cannot_activate_recovered_session() {
        run("old-auth");
    }
    #[test]
    fn current_auth_does_not_make_old_selection_current() {
        run("old-selection");
    }
    #[test]
    fn uncertain_effect_closes_session_and_is_never_retried() {
        run("uncertain");
    }

    use yoohoo_runtime::controller::{
        Controller, Operation, Phase, Reply as ControlReply, Request as ControlRequest, Startup,
    };
    fn control(
        fixture: &Fixture,
        operation: Operation,
        observed: Option<&ControlReply>,
    ) -> ControlReply {
        let mut peer = modal_client::data::Client::connect(
            &fixture.dir.path().join("control/modal.sock"),
            identity(),
        )
        .unwrap();
        let request = ControlRequest {
            version: 1,
            instance: observed.map(|r| r.instance),
            revision: observed.map_or(0, |r| r.revision),
            operation,
        };
        let bytes = peer
            .exchange(
                &serde_json::to_vec(&request).unwrap(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    fn wait_phase(fixture: &Fixture, phase: Phase) -> ControlReply {
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            let status = control(fixture, Operation::Status, None);
            if status.phase == phase {
                return status;
            }
            assert_ne!(
                status.phase,
                Phase::Faulted,
                "unexpected terminal fault while waiting for {phase:?}"
            );
            assert!(Instant::now() < end, "{status:?}");
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn controller_scenario(mode: &str) {
        let fixture = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let uncertain = Arc::new(AtomicBool::new(false));
        let bad = Arc::new(AtomicBool::new(false));
        let grants = Arc::new(Mutex::new(Vec::new()));
        let mut server = host_observed(
            &fixture,
            calls.clone(),
            uncertain.clone(),
            bad.clone(),
            grants.clone(),
        );
        for name in ["data", "control"] {
            std::fs::create_dir(fixture.dir.path().join(name)).unwrap();
            std::fs::set_permissions(
                fixture.dir.path().join(name),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let pid = identity();
        let config = Startup {
            modal_root: fixture.dir.path().join("arbiter"),
            native_root: fixture.dir.path().into(),
            native_instance: "test".into(),
            host_pid: pid.pid,
            host_start_ticks: pid.start_ticks,
            native_pid: pid.pid,
            native_start_ticks: pid.start_ticks,
        };
        let mut controller = Controller::new(
            config,
            &fixture.dir.path().join("data"),
            Policy {
                initial: Duration::from_millis(20),
                maximum: Duration::from_millis(40),
                jitter: Duration::ZERO,
                ..Policy::default()
            },
        )
        .unwrap();
        let initial = controller.status();
        assert_eq!(initial.phase, Phase::Waiting);
        assert!(
            !controller
                .command(ControlRequest {
                    version: 1,
                    instance: Some(initial.instance),
                    revision: initial.revision,
                    operation: Operation::Open
                })
                .unwrap()
                .accepted
        );
        assert!(grants.lock().unwrap().is_empty());
        let listener =
            modal_runtime::socket::ControlSocket::bind(&fixture.dir.path().join("control"))
                .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        let handle = thread::spawn(move || {
            while !halt.load(Ordering::Acquire) && controller.status().phase != Phase::Stopped {
                controller.serve_step(&listener).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let mut controller = Thread {
            stop,
            handle: Some(handle),
        };
        let before = wait_phase(&fixture, Phase::Ready);
        if mode == "positive" {
            for frame in [b"{".to_vec(), b"{\"version\":1,\"instance\":null,\"revision\":0,\"operation\":\"dispatch\"}\n".to_vec(), b"{\"version\":1,\"version\":1,\"instance\":null,\"revision\":0,\"operation\":\"status\"}\n".to_vec(), vec![b'x';5000]] {
                let mut peer=UnixStream::connect(fixture.dir.path().join("control/modal.sock")).unwrap();
                peer.set_write_timeout(Some(Duration::from_millis(100))).unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                peer.write_all(&frame).unwrap();
                let end=peer.read(&mut [0;1]);
                assert!(matches!(end,Ok(0)) || matches!(end,Err(e) if e.kind()==std::io::ErrorKind::ConnectionReset));
            }
            let mut foreign = control(&fixture, Operation::Status, None);
            foreign.instance[0] ^= 1;
            assert!(!control(&fixture, Operation::Open, Some(&foreign)).accepted);
            assert_eq!(
                control(&fixture, Operation::Status, None).phase,
                Phase::Ready
            );
        }
        assert!(
            grants.lock().unwrap().is_empty(),
            "source readiness auto-opened UI"
        );
        fixture.edge(b"urgent>>0x1\n");
        thread::sleep(Duration::from_millis(5));
        let first = control(&fixture, Operation::Status, None);
        assert!(
            !control(&fixture, Operation::Open, Some(&before)).accepted,
            "stale control precondition opened"
        );
        let status = control(&fixture, Operation::Status, None);
        let opened = control(&fixture, Operation::Open, Some(&status));
        assert!(opened.accepted);
        assert_eq!(opened.phase, Phase::Active);
        assert!(!opened.cleanup_confirmed);
        assert!(
            !control(&fixture, Operation::Open, Some(&status)).accepted,
            "Open replay accepted"
        );
        let busy = control(&fixture, Operation::Status, None);
        assert!(
            !control(&fixture, Operation::Open, Some(&busy)).accepted,
            "Open while active accepted"
        );
        let fence1 = *grants.lock().unwrap().last().unwrap();
        let auth1 = modal_client::presenter::Auth::new(
            fence1,
            format!("omarchy-modal-{:032x}", fence1.generation.get()),
        )
        .unwrap();
        let mut link = Link::connect(
            &fixture.dir.path().join("data/modal.sock"),
            identity(),
            auth1.clone(),
        )
        .unwrap();
        let model = link.model().unwrap();
        assert_eq!(model.view.rows.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        if mode == "uncertain" {
            uncertain.store(true, Ordering::Release);
            assert!(link.apply(activation(&model)).is_err());
            let failed = wait_phase(&fixture, Phase::Faulted);
            assert!(!failed.cleanup_confirmed);
            assert!(!control(&fixture, Operation::Open, Some(&failed)).accepted);
            assert!(link.apply(activation(&model)).is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else if mode == "quit-bad" || mode == "quit-good" {
            bad.store(mode == "quit-bad", Ordering::Release);
            let status = control(&fixture, Operation::Status, None);
            let result = control(&fixture, Operation::Quit, Some(&status));
            assert_eq!(
                result.accepted,
                mode == "quit-good",
                "Quit cleanup outcome misreported"
            );
            assert_eq!(result.cleanup_confirmed, mode == "quit-good");
            assert_eq!(
                result.phase,
                if mode == "quit-good" {
                    Phase::Stopped
                } else {
                    Phase::Faulted
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                link.apply(Command::Close {
                    generation: model.view.generation
                })
                .unwrap()
            );
            drop(link);
            let idle = wait_phase(&fixture, Phase::Ready);
            assert!(idle.cleanup_confirmed);
            fixture.mode.store(1, Ordering::Release);
            fixture.disconnect();
            let waiting = wait_phase(&fixture, Phase::Waiting);
            assert!(!control(&fixture, Operation::Open, Some(&waiting)).accepted);
            fixture.mode.store(0, Ordering::Release);
            wait_phase(&fixture, Phase::Ready);
            fixture.edge(b"urgent>>0x1\n");
            thread::sleep(Duration::from_millis(5));
            let status = control(&fixture, Operation::Status, None);
            assert!(control(&fixture, Operation::Open, Some(&status)).accepted);
            let fence2 = *grants.lock().unwrap().last().unwrap();
            assert_ne!(fence1, fence2);
            assert!(!control(&fixture, Operation::Open, Some(&first)).accepted);
            if mode == "old-presenter" {
                let mut old = Link::connect(
                    &fixture.dir.path().join("data/modal.sock"),
                    identity(),
                    auth1,
                )
                .unwrap();
                assert!(old.model().is_err());
                assert_eq!(wait_phase(&fixture, Phase::Faulted).phase, Phase::Faulted);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            } else {
                let auth2 = modal_client::presenter::Auth::new(
                    fence2,
                    format!("omarchy-modal-{:032x}", fence2.generation.get()),
                )
                .unwrap();
                let mut current = Link::connect(
                    &fixture.dir.path().join("data/modal.sock"),
                    identity(),
                    auth2,
                )
                .unwrap();
                let next = current.model().unwrap();
                assert!(current.apply(activation(&next)).unwrap());
                assert!(wait_phase(&fixture, Phase::Ready).cleanup_confirmed);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                let status = control(&fixture, Operation::Status, None);
                assert!(control(&fixture, Operation::Quit, Some(&status)).accepted);
            }
        }
        controller.finish();
        server.finish();
    }
    #[test]
    fn controller_explicit_two_opens_idle_recovery_and_fresh_focus() {
        controller_scenario("positive");
    }
    #[test]
    fn controller_old_presenter_cannot_attach_after_clean_handback() {
        controller_scenario("old-presenter");
    }
    #[test]
    fn controller_uncertain_effect_never_reopens() {
        controller_scenario("uncertain");
    }
    #[test]
    fn controller_quit_active_requires_confirmed_dismissal() {
        controller_scenario("quit-bad");
    }
    #[test]
    fn controller_quit_active_reports_confirmed_cleanup() {
        controller_scenario("quit-good");
    }

    struct ChildGuard {
        child: std::process::Child,
        done: bool,
    }
    impl ChildGuard {
        fn wait(&mut self) -> std::process::ExitStatus {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    self.done = true;
                    return status;
                }
                assert!(Instant::now() < end, "fixture child did not finish");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !self.done {
                match self.child.try_wait() {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                    }
                    Err(_) => {}
                }
            }
        }
    }
    fn logged_spawn(command: &mut std::process::Command, log: &std::path::Path) -> ChildGuard {
        use std::os::unix::fs::OpenOptionsExt;
        let output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log)
            .unwrap();
        let child = command
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        ChildGuard { child, done: false }
    }
    fn text(log: &std::path::Path) -> String {
        let mut out = String::new();
        std::fs::File::open(log)
            .unwrap()
            .take(4096)
            .read_to_string(&mut out)
            .unwrap();
        out
    }
    fn cli(
        fixture: &Fixture,
        process: desktop_io::ProcessIdentity,
        mode: &str,
        count: u32,
    ) -> ControlReply {
        let path = fixture.dir.path().join(format!("cli-{count}.log"));
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_yoohood"));
        command
            .args([
                mode,
                fixture
                    .dir
                    .path()
                    .join("control/modal.sock")
                    .to_str()
                    .unwrap(),
                &process.pid.to_string(),
                &process.start_ticks.to_string(),
            ])
            .stdin(std::process::Stdio::null());
        let mut child = logged_spawn(&mut command, &path);
        assert!(child.wait().success(), "{}", text(&path));
        serde_json::from_str(text(&path).trim()).unwrap()
    }
    #[test]
    fn actual_daemon_cli_status_open_close_reopen_and_quit() {
        let fixture = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(AtomicUsize::new(0));
        let mut server = host_for_peer(
            &fixture,
            calls.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            grants.clone(),
            allowed.clone(),
        );
        for name in ["data", "control"] {
            std::fs::create_dir(fixture.dir.path().join(name)).unwrap();
            std::fs::set_permissions(
                fixture.dir.path().join(name),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let log = fixture.dir.path().join("daemon.log");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_yoohood"));
        command
            .arg("serve")
            .arg(fixture.dir.path().join("data"))
            .arg(fixture.dir.path().join("control"))
            .stdin(std::process::Stdio::piped());
        let mut daemon = logged_spawn(&mut command, &log);
        allowed.store(daemon.child.id() as usize, Ordering::Release);
        let end = Instant::now() + Duration::from_secs(2);
        while !text(&log).contains("controller_ready=true") {
            assert!(Instant::now() < end);
            assert!(daemon.child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(2));
        }
        let peer = identity();
        let raw = std::fs::read_to_string(format!("/proc/{}/stat", daemon.child.id())).unwrap();
        let process = desktop_io::ProcessIdentity {
            pid: daemon.child.id(),
            start_ticks: raw
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .parse()
                .unwrap(),
        };
        let config = serde_json::json!({"modal_root":fixture.dir.path().join("arbiter"),"native_root":fixture.dir.path(),"native_instance":"test","host_pid":peer.pid,"host_start_ticks":peer.start_ticks,"native_pid":peer.pid,"native_start_ticks":peer.start_ticks});
        writeln!(daemon.child.stdin.take().unwrap(), "{config}").unwrap();
        let mut count = 0;
        loop {
            count += 1;
            let status = cli(&fixture, process, "status", count);
            if status.phase == Phase::Ready {
                break;
            }
            assert!(Instant::now() < end);
        }
        assert!(
            grants.lock().unwrap().is_empty(),
            "daemon auto-opened before command"
        );
        count += 1;
        let before = cli(&fixture, process, "status", count);
        let mut invalid =
            UnixStream::connect(fixture.dir.path().join("control/modal.sock")).unwrap();
        invalid
            .set_write_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        invalid
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        invalid
            .write_all(
                b"{\"version\":2,\"instance\":null,\"revision\":0,\"operation\":\"status\"}\n",
            )
            .unwrap();
        let mut byte = [0u8; 1];
        let refusal = invalid.read(&mut byte);
        assert!(
            matches!(refusal, Ok(0))
                || matches!(refusal, Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset)
        );
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "unsupported request terminated daemon"
        );
        count += 1;
        let after = cli(&fixture, process, "status", count);
        assert_eq!(after.phase, Phase::Ready);
        assert_eq!(after.instance, before.instance);
        assert_eq!(after.revision, before.revision);
        assert!(!after.cleanup_confirmed);
        assert!(grants.lock().unwrap().is_empty());
        fixture.edge(b"urgent>>0x1\n");
        count += 1;
        let opened = cli(&fixture, process, "open", count);
        assert_eq!(opened.phase, Phase::Active);
        let first = *grants.lock().unwrap().last().unwrap();
        let auth = modal_client::presenter::Auth::new(
            first,
            format!("omarchy-modal-{:032x}", first.generation.get()),
        )
        .unwrap();
        let mut link =
            Link::connect(&fixture.dir.path().join("data/modal.sock"), process, auth).unwrap();
        let model = link.model().unwrap();
        assert!(
            link.apply(Command::Close {
                generation: model.view.generation
            })
            .unwrap()
        );
        drop(link);
        loop {
            count += 1;
            if cli(&fixture, process, "status", count).phase == Phase::Ready {
                break;
            }
            assert!(Instant::now() < end);
        }
        count += 1;
        assert_eq!(cli(&fixture, process, "open", count).phase, Phase::Active);
        let second = *grants.lock().unwrap().last().unwrap();
        assert_ne!(first, second);
        count += 1;
        let quit = cli(&fixture, process, "quit", count);
        assert_eq!(quit.phase, Phase::Stopped);
        assert!(quit.cleanup_confirmed);
        assert!(daemon.wait().success());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!fixture.dir.path().join("control/modal.sock").exists());
        assert!(!fixture.dir.path().join("data/modal.sock").exists());
        server.finish();
    }
}
