use desktop_io::ProcessIdentity;
use modal_client::{Client, Error};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
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
fn grant(id: &Value) -> Value {
    json!({"version":1,"request_id":id,"status":"granted","epoch":([1;16]),"generation":"1","expires_ms":"5000","client_id":([2;16]),"client_epoch":"1"})
}
fn fake(
    responses: Vec<Box<dyn FnOnce(Value) -> Vec<u8> + Send>>,
) -> (tempfile::TempDir, thread::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(dir.path().join("modal.sock")).unwrap();
    std::fs::set_permissions(
        dir.path().join("modal.sock"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let thread = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        for response in responses {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let reply = response(serde_json::from_str(&line).unwrap());
            peer.write_all(&reply).unwrap();
        }
    });
    (dir, thread)
}
fn bytes(value: Value) -> Vec<u8> {
    let mut v = serde_json::to_vec(&value).unwrap();
    v.push(b'\n');
    v
}
#[test]
fn duplicate_reply_fields_poison_connection_and_clear_lease() {
    let (dir, server) = fake(vec![Box::new(|_| {
        br#"{"version":1,"version":1,"request_id":"1","status":"complete"}
"#
        .to_vec()
    })]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    assert!(matches!(
        client.acquire(Duration::from_secs(2)),
        Err(Error::Protocol)
    ));
    assert!(client.lease().is_none());
    assert!(matches!(
        client.acquire(Duration::from_secs(2)),
        Err(Error::Unavailable)
    ));
    server.join().unwrap();
}
#[test]
fn partial_reply_eof_is_not_a_grant() {
    let (dir, server) = fake(vec![Box::new(|_| b"{\"version\":1".to_vec())]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    assert!(matches!(
        client.acquire(Duration::from_secs(2)),
        Err(Error::Transport)
    ));
    assert!(client.lease().is_none());
    server.join().unwrap();
}
#[test]
fn changed_revision_during_paging_discards_entire_view() {
    let page = |revision: &str, id: Value, token: &str, next: Option<usize>| json!({"version":1,"request_id":id,"status":"targets","revision":revision,"producer_session":([3;16]),"next":next,"rows":[{"token":token,"instance":"fixture","stable_id":revision}]});
    let (dir, server) = fake(vec![
        Box::new(|r| bytes(grant(&r["request_id"]))),
        Box::new(move |r| bytes(page("1", r["request_id"].clone(), &"a".repeat(48), Some(1)))),
        Box::new(move |r| bytes(page("2", r["request_id"].clone(), &"b".repeat(48), None))),
    ]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    client.acquire(Duration::from_secs(2)).unwrap();
    assert!(matches!(client.targets(), Err(Error::Protocol)));
    assert!(client.lease().is_none());
    server.join().unwrap();
}
#[test]
fn process_start_mismatch_is_rejected_before_connect() {
    let dir = tempfile::tempdir().unwrap();
    let mut wrong = identity();
    wrong.start_ticks += 1;
    assert!(matches!(
        Client::connect(dir.path(), wrong),
        Err(Error::Transport)
    ));
}
#[test]
fn oversized_frame_is_rejected_before_json_parse() {
    let (dir, server) = fake(vec![Box::new(|_| vec![b'x'; 4097])]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    assert!(matches!(
        client.acquire(Duration::from_secs(2)),
        Err(Error::Transport)
    ));
    server.join().unwrap();
}
#[test]
fn actual_host_full_registry_paging_focus_and_release() {
    use modal_contract::{Fence, Owner};
    use modal_runtime::{
        actor::{Application, Backend, Dismissal, Effect, Error as HostError},
        host::Host,
        targets::NativeTarget,
    };
    struct Fixture(Arc<std::sync::Mutex<Vec<String>>>);
    impl Backend for Fixture {
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
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let focused = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = focused.clone();
    let mut host = Host::bind(dir.path(), Fixture(focused), |_, _| {
        Some(Application::Vimarchy)
    })
    .unwrap();
    let ticket = host.begin_source().unwrap();
    host.snapshot(
        ticket,
        1,
        (0..25)
            .map(|i| NativeTarget::new("fixture", &format!("{i:x}")).unwrap())
            .collect(),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let server = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(3);
        while !stopped.load(Ordering::Acquire) && Instant::now() < end {
            host.step().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    let lease = client.acquire(Duration::from_secs(2)).unwrap();
    assert_ne!(lease.fence.owner.client.0, [0; 16]);
    let view = client.targets().unwrap();
    assert_eq!(view.rows().len(), 25);
    let target = view.rows()[8].target.stable_id().to_owned();
    client.focus(&view, 8).unwrap();
    assert_eq!(*observed.lock().unwrap(), vec![target]);
    client.release().unwrap();
    assert!(client.lease().is_none());
    stop.store(true, Ordering::Release);
    server.join().unwrap();
}

#[test]
fn focus_reply_loss_is_unconfirmed_and_never_retried() {
    let (dir, server) = fake(vec![
        Box::new(|r| bytes(grant(&r["request_id"]))),
        Box::new(|r| {
            bytes(
                json!({"version":1,"request_id":r["request_id"],"status":"targets","revision":"1","producer_session":([3;16]),"next":null,"rows":[{"token":"a".repeat(48),"instance":"fixture","stable_id":"1"}]}),
            )
        }),
        Box::new(|r| {
            assert_eq!(r["command"]["op"], "focus_window");
            b"{\"version\":".to_vec()
        }),
    ]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    client.acquire(Duration::from_secs(2)).unwrap();
    let view = client.targets().unwrap();
    assert_eq!(client.focus(&view, 0), Err(Error::EffectUnconfirmed));
    assert!(client.lease().is_none());
    assert!(client.focus(&view, 0).is_err());
    server.join().unwrap();
}

#[test]
fn closed_transition_and_release_reply_loss_are_unconfirmed() {
    for operation in ["execute", "release"] {
        let (dir, server) = fake(vec![
            Box::new(|r| bytes(grant(&r["request_id"]))),
            Box::new(move |r| {
                assert_eq!(r["command"]["op"], operation);
                b"{\"version\":".to_vec()
            }),
        ]);
        let mut client = Client::connect(dir.path(), identity()).unwrap();
        client.acquire(Duration::from_secs(2)).unwrap();
        let result = if operation == "execute" {
            client.execute(modal_runtime::actor::Effect::EnterVimarchy)
        } else {
            client.release()
        };
        assert_eq!(result, Err(Error::EffectUnconfirmed));
        assert!(client.lease().is_none());
        server.join().unwrap();
    }
}

#[test]
fn presenter_identity_is_closed_and_cannot_change_on_renewal() {
    for namespace in [
        "omarchy-modal-invalid".to_string(),
        format!("omarchy-modal-{}", "A".repeat(32)),
    ] {
        let (dir, server) = fake(vec![Box::new(move |r| {
            let mut g = grant(&r["request_id"]);
            g["presenter"] = json!({"pid":123,"start_ticks":"1","namespace":namespace});
            bytes(g)
        })]);
        let mut client = Client::connect(dir.path(), identity()).unwrap();
        assert!(matches!(
            client.acquire(Duration::from_secs(2)),
            Err(Error::Protocol)
        ));
        assert!(client.lease().is_none());
        server.join().unwrap();
    }
    let response = |pid| {
        Box::new(move |r: Value| {
            let mut g = grant(&r["request_id"]);
            g["presenter"] = json!({"pid":pid,"start_ticks":"1","namespace":format!("omarchy-modal-{}", "a".repeat(32))});
            bytes(g)
        }) as Box<dyn FnOnce(Value) -> Vec<u8> + Send>
    };
    let (dir, server) = fake(vec![response(123), response(124)]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    let lease = client.acquire(Duration::from_secs(2)).unwrap();
    assert_eq!(lease.presenter.unwrap().process().pid, 123);
    assert!(client.renew(Duration::from_secs(2)).is_err());
    assert!(client.lease().is_none());
    server.join().unwrap();
}

#[test]
fn process_guard_rejects_wrong_peer_start_and_exited_child() {
    use modal_client::data::ProcessGuard;
    let own = identity();
    let guard = ProcessGuard::pin(own).unwrap();
    assert!(
        guard
            .check_peer(unsafe { libc::geteuid() }, own.pid as i32)
            .is_ok()
    );
    assert!(guard.check_peer(unsafe { libc::geteuid() }, -1).is_err());
    let mut wrong = own;
    wrong.start_ticks += 1;
    assert!(ProcessGuard::pin(wrong).is_err());
    let mut child = std::process::Command::new("/usr/bin/cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
    let start = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let child_guard = ProcessGuard::pin(ProcessIdentity {
        pid: child.id(),
        start_ticks: start,
    })
    .unwrap();
    drop(child.stdin.take());
    child.wait().unwrap();
    assert!(child_guard.check().is_err());
}

#[test]
fn data_guard_and_transport_reject_same_pid_executable_replacement() {
    use modal_client::data::{Client as DataClient, ProcessGuard};
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = r#"
import os,socket,sys
listener=socket.socket(socket.AF_UNIX);listener.bind(sys.argv[1]);os.chmod(sys.argv[1],0o600);listener.listen(1)
print('ready',flush=True)
peer,_=listener.accept();os.set_inheritable(peer.fileno(),True)
request=b''
while not request.endswith(b'\n'):request+=peer.recv(1)
assert request==b'ping\n';peer.sendall(b'pong\n')
assert sys.stdin.readline().strip()=='exec'
os.execv('/usr/bin/sleep',['sleep','30'])
"#;
    let mut child = Command::new("/usr/bin/python")
        .args(["-c", script])
        .arg(dir.path().join("modal.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
    let start_ticks = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let process = ProcessIdentity {
        pid: child.id(),
        start_ticks,
    };
    let guard = ProcessGuard::pin(process).unwrap();
    let mut client = DataClient::connect(&dir.path().join("modal.sock"), process).unwrap();
    assert_eq!(
        client
            .exchange(b"ping", Instant::now() + Duration::from_millis(500))
            .unwrap(),
        b"pong"
    );
    guard.check().unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "exec").unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while std::fs::read_link(format!("/proc/{}/exe", child.id())).unwrap()
        != std::path::Path::new("/usr/bin/sleep")
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let guard_rejected = guard.check().is_err();
    let transport_rejected = client
        .exchange(
            b"must-not-send",
            Instant::now() + Duration::from_millis(500),
        )
        .is_err();
    let poisoned = client
        .exchange(b"never-retry", Instant::now() + Duration::from_millis(500))
        .is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(guard_rejected && transport_rejected && poisoned);
}

#[test]
fn coalesced_unsolicited_reply_frames_are_refused() {
    let (dir, server) = fake(vec![Box::new(|r| {
        let first = bytes(grant(&r["request_id"]));
        [first.clone(), first].concat()
    })]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    assert!(client.acquire(Duration::from_secs(2)).is_err());
    assert!(client.lease().is_none());
    server.join().unwrap();
}

#[test]
fn maximize_reply_loss_is_unconfirmed_and_never_retried() {
    let (dir, server) = fake(vec![
        Box::new(|r| bytes(grant(&r["request_id"]))),
        Box::new(|r| {
            bytes(
                json!({"version":1,"request_id":r["request_id"],"status":"targets","revision":"1","producer_session":([3;16]),"next":null,"rows":[{"token":"a".repeat(48),"instance":"fixture","stable_id":"1"}]}),
            )
        }),
        Box::new(|r| {
            assert_eq!(r["command"]["op"], "maximize_window");
            b"{\"version\":".to_vec()
        }),
    ]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    client.acquire(Duration::from_secs(2)).unwrap();
    let view = client.targets().unwrap();
    let request = modal_runtime::vimarchy_action::MaximizeRequest {
        workspace: 2,
        layout: "dwindle".into(),
        fullscreen: 0,
        action: modal_runtime::vimarchy_action::MaximizeAction::Set,
        alt: false,
        policy_digest: modal_runtime::vimarchy_policy::Policy::default().digest(),
    };
    assert_eq!(
        client.maximize(&view, 0, &request),
        Err(Error::EffectUnconfirmed)
    );
    assert!(client.lease().is_none());
    assert!(client.maximize(&view, 0, &request).is_err());
    server.join().unwrap();
}

#[test]
fn workspace_reply_loss_is_unconfirmed_and_never_retried() {
    let (dir, server) = fake(vec![
        Box::new(|r| bytes(grant(&r["request_id"]))),
        Box::new(|r| {
            bytes(
                json!({"version":1,"request_id":r["request_id"],"status":"targets","revision":"1","producer_session":([3;16]),"next":null,"rows":[{"token":"a".repeat(48),"instance":"fixture","stable_id":"1"}]}),
            )
        }),
        Box::new(|r| {
            assert_eq!(r["command"]["op"], "move_workspace");
            b"{\"version\":".to_vec()
        }),
    ]);
    let mut client = Client::connect(dir.path(), identity()).unwrap();
    client.acquire(Duration::from_secs(2)).unwrap();
    let view = client.targets().unwrap();
    let workspace = |id| modal_runtime::workspace_move::WorkspaceIdentity {
        id,
        name: id.to_string(),
        monitor_id: 0,
    };
    let request = modal_runtime::workspace_move::MoveWorkspaceRequest {
        source: workspace(2),
        destination: workspace(3),
        active: workspace(2),
        follow: false,
    };
    assert_eq!(
        client.move_workspace(&view, 0, &request),
        Err(Error::EffectUnconfirmed)
    );
    assert!(client.lease().is_none());
    assert!(client.move_workspace(&view, 0, &request).is_err());
    server.join().unwrap();
}
