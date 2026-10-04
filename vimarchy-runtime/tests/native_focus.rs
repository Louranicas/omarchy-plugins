use desktop_io::{Endpoint, ProcessIdentity};
use modal_contract::{Fence, Owner};
use modal_runtime::{
    actor::{Application, Backend, Dismissal, Effect, Error as HostError},
    host::Host,
    targets::NativeTarget,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use vimarchy_core::gestures::{Effect as GestureEffect, Event};
use vimarchy_runtime::{Context, Session, native::Native};
fn identity() -> ProcessIdentity {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    ProcessIdentity {
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
struct BackendFixture(Arc<Mutex<Vec<String>>>);
impl Backend for BackendFixture {
    fn register(&mut self, _: Owner, app: Application) -> Result<(), HostError> {
        assert_eq!(app, Application::Vimarchy);
        Ok(())
    }
    fn present(&mut self, _: Fence, _: u64) -> Result<(), HostError> {
        Ok(())
    }
    fn dismiss(&mut self, _: Fence) -> Result<Dismissal, HostError> {
        Ok(Dismissal {
            presenter_exited: true,
            compositor_dismissed: true,
        })
    }
    fn dispatch(&mut self, _: Fence, _: u64, _: Effect) -> Result<(), HostError> {
        Ok(())
    }
    fn focus(&mut self, _: Fence, _: u64, target: &NativeTarget) -> Result<(), HostError> {
        self.0.lock().unwrap().push(target.stable_id().into());
        Ok(())
    }
}
#[test]
fn desktop_rows_core_gesture_registry_focus_and_event_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(dir.path().join("hypr/fixture_1")).unwrap();
    let listener = UnixListener::bind(dir.path().join("hypr/fixture_1/.socket.sock")).unwrap();
    let events = UnixListener::bind(dir.path().join("hypr/fixture_1/.socket2.sock")).unwrap();
    let (send, receive) = mpsc::channel::<bool>();
    let (done, ack) = mpsc::channel();
    let event_thread = thread::spawn(move || {
        let (mut peer, _) = events.accept().unwrap();
        while let Ok(close) = receive.recv_timeout(Duration::from_secs(3)) {
            if !close {
                break;
            }
            peer.write_all(b"closewindow>>1\n").unwrap();
            done.send(()).unwrap();
        }
    });
    let query_thread = thread::spawn(move || {
        for (wire, reply) in [
            (
                "j/clients",
                r#"[{"address":"0x1","stableId":"AB","mapped":true,"hidden":false,"workspace":{"id":1},"at":[10,10],"size":[400,300],"grouped":[],"focusHistoryID":0}]"#,
            ),
            (
                "j/monitors",
                r#"[{"name":"fixture","x":0,"y":0,"width":1280,"height":720,"scale":1,"activeWorkspace":{"id":1},"specialWorkspace":{"id":0}}]"#,
            ),
            (
                "j/clients",
                r#"[{"stableId":"ab","mapped":true,"hidden":false,"workspace":{"id":1},"fullscreen":0}]"#,
            ),
            ("j/workspaces", r#"[{"id":1,"tiledLayout":"dwindle"}]"#),
        ] {
            let (mut peer, _) = listener.accept().unwrap();
            let mut b = vec![0; wire.len()];
            peer.read_exact(&mut b).unwrap();
            assert_eq!(b, wire.as_bytes());
            peer.write_all(reply.as_bytes()).unwrap();
        }
    });
    let endpoint = Endpoint::discover_identity(dir.path(), "fixture_1", identity()).unwrap();
    let mut native = Native::connect(endpoint, [4; 16]).unwrap();
    let observation = native.observe().unwrap();
    assert_eq!(observation.clients()[0].stable_id, "ab");
    let modal = dir.path().join("modal");
    std::fs::create_dir(&modal).unwrap();
    std::fs::set_permissions(&modal, std::fs::Permissions::from_mode(0o700)).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut host = Host::bind(&modal, BackendFixture(seen.clone()), |_, _| {
        Some(Application::Vimarchy)
    })
    .unwrap();
    let source = host.begin_source().unwrap();
    host.snapshot(
        source,
        1,
        vec![NativeTarget::new("fixture_1", "ab").unwrap()],
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let host_thread = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(3);
        while !stopped.load(Ordering::Acquire) && Instant::now() < end {
            host.step().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });
    let origin = Instant::now();
    let mut arbiter = modal_client::Client::connect(&modal, identity()).unwrap();
    let lease = arbiter.acquire(Duration::from_secs(2)).unwrap();
    let view = arbiter.targets().unwrap();
    let binding = native.bind(&observation, view).unwrap();
    let context = Context {
        presenter: [7; 16],
        fence: lease.fence,
        source_revision: observation.revision(),
    };
    let now = || origin.elapsed().as_millis() as u64;
    let (mut session, _) = Session::open(
        context,
        lease.deadline.duration_since(origin).as_millis() as u64,
        now(),
        observation.monitors(),
        observation.clients(),
        &BTreeMap::new(),
        250,
    )
    .unwrap();
    session.host_ready(context, now()).unwrap(); // Explicit test backend proof, no native claim.
    session
        .input(
            context,
            Event::Down {
                hint: "a".into(),
                press_id: 1,
                alt: false,
                repeat: false,
            },
            now(),
        )
        .unwrap();
    let batch = session
        .input(context, Event::Up { press_id: 1 }, now())
        .unwrap();
    let effects = session.consume_batch(batch, now()).unwrap();
    let target = effects
        .iter()
        .find_map(|e| {
            if let GestureEffect::Focus(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .unwrap();
    let plan = native
        .plan_double_tap(
            &binding,
            target,
            true,
            &vimarchy_runtime::policy::Policy::default(),
        )
        .unwrap();
    let vimarchy_runtime::action_plan::Decision::Maximize(plan) = plan else {
        panic!("expected native proposal")
    };
    assert_eq!(plan.target().stable_id(), "ab");
    assert_eq!(plan.expected_fullscreen(), 1);
    assert!(seen.lock().unwrap().is_empty()); // Planning performs no backend effect.
    native.focus(&mut arbiter, &binding, target).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["ab"]);
    send.send(true).unwrap();
    ack.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        native
            .plan_double_tap(
                &binding,
                target,
                true,
                &vimarchy_runtime::policy::Policy::default()
            )
            .is_err()
    );
    assert!(native.focus(&mut arbiter, &binding, target).is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);
    arbiter.release().unwrap();
    stop.store(true, Ordering::Release);
    send.send(false).unwrap();
    event_thread.join().unwrap();
    query_thread.join().unwrap();
    host_thread.join().unwrap();
}
