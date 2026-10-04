//! Real Client → Host → PresenterBackend → NativeCompositor → fake Hyprland IPC.
//! Presenter map/dismissal are simulated; no installed compositor is contacted.
use desktop_io::{DispatchDialect, ProcessIdentity};
use modal_client::Client;
use modal_contract::Fence;
use modal_runtime::{
    actor::{Application, Effect, Error},
    backend::{Compositor, PresenterBackend, PresenterSpec},
    host::Host,
    native::{NativeCompositor, NativeFocus, SUPPORTED_COMMIT},
    vimarchy_action::{MaximizeAction, MaximizeRequest},
    vimarchy_policy::Policy,
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
fn identity() -> ProcessIdentity {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
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
struct Lifecycle;
impl Compositor for Lifecycle {
    fn ready(&mut self, _: u32, _: Fence, _: Instant) -> Result<bool, Error> {
        Ok(true)
    }
    fn dismissed(&mut self, _: Fence, _: Instant) -> Result<bool, Error> {
        Ok(true)
    }
    fn dispatch(&mut self, _: Fence, _: Effect, _: Instant) -> Result<(), Error> {
        Ok(())
    }
}
#[derive(Default)]
struct State {
    fullscreen: u8,
    submitted: Vec<String>,
    lose_ack: bool,
}
fn scenario(case: &str) {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    let modal = temp.path().join("modal");
    for path in [&native, &modal] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir_all(native.join("hypr/fixture")).unwrap();
    let commands = UnixListener::bind(native.join("hypr/fixture/.socket.sock")).unwrap();
    commands.set_nonblocking(true).unwrap();
    let events = UnixListener::bind(native.join("hypr/fixture/.socket2.sock")).unwrap();
    events.set_nonblocking(true).unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    let observed = state.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let io = thread::spawn(move || {
        let mut event_peers = Vec::new();
        while !stopped.load(Ordering::Acquire) {
            if let Ok((stream, _)) = events.accept() {
                event_peers.push(stream);
            }
            if let Ok((mut stream, _)) = commands.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut b = [0; 2048];
                let n = stream.read(&mut b).unwrap();
                let command = std::str::from_utf8(&b[..n]).unwrap();
                let mut state = observed.lock().unwrap();
                let reply=match command {
    "j/version"=>json!({"commit":SUPPORTED_COMMIT,"dirty":false}).to_string(),
    "j/clients"=>json!([{"stableId":"ab","mapped":true,"hidden":false,"workspace":{"id":2},"fullscreen":state.fullscreen}]).to_string(),
    "j/workspaces"=>json!([{"id":2,"tiledLayout":"dwindle"}]).to_string(),
    s if s.starts_with("/dispatch hl.dsp.window.fullscreen(")=> {state.submitted.push(s.into());state.fullscreen=if s.contains("action=\"set\""){1}else{0};if state.lose_ack {String::new()}else{"ok".into()}},
    _=>panic!("unexpected command {command}"),
   };
                stream.write_all(reply.as_bytes()).unwrap();
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let policy_path = temp.path().join("settings.json");
    let raw = match case {
        "disabled" => r#"{"doubleTap":{"layouts":{"dwindle":"disabled"}}}"#,
        "custom" => r#"{"doubleTap":{"layouts":{"dwindle":["custom"]}}}"#,
        _ => "{}",
    };
    fs::write(&policy_path, raw).unwrap();
    let policy = Policy::load(&policy_path).unwrap();
    let server_policy = if case == "omitted" {
        None
    } else {
        Some(policy.clone())
    };
    let path = policy_path.clone();
    let root = modal.clone();
    let stopped = stop.clone();
    let foreign = case == "foreign_role";
    let stale = case == "stale";
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (invalidate_tx, invalidate_rx) = std::sync::mpsc::channel();
    let (invalidated_tx, invalidated_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let focus = NativeFocus::connect(
            &native,
            "fixture",
            identity(),
            DispatchDialect::Lua,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let mut observer = focus.observer();
        let specs = std::array::from_fn(|_| PresenterSpec {
            program: "/usr/bin/sleep".into(),
            arguments: vec!["20".into()],
            environment: vec![],
        });
        let backend = PresenterBackend::new(
            NativeCompositor {
                lifecycle: Lifecycle,
                focus,
            },
            specs,
        )
        .with_vimarchy_policy(server_policy)
        .with_vimarchy_policy_path(Some(path));
        let mut host = Host::bind(&root, backend, move |_, _| {
            Some(if foreign {
                Application::Yoohoo
            } else {
                Application::Vimarchy
            })
        })
        .unwrap();
        observer
            .step(&mut host, Instant::now() + Duration::from_secs(1))
            .unwrap();
        ready_tx.send(()).unwrap();
        while !stopped.load(Ordering::Acquire) {
            if stale && invalidate_rx.try_recv().is_ok() {
                let ticket = host.begin_source().unwrap();
                host.snapshot(
                    ticket,
                    1,
                    vec![modal_runtime::targets::NativeTarget::new("fixture", "ab").unwrap()],
                )
                .unwrap();
                invalidated_tx.send(()).unwrap();
            }
            let _ = host.step();
            thread::sleep(Duration::from_millis(1));
        }
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut client = Client::connect(&modal, identity()).unwrap();
    client.acquire(Duration::from_secs(3)).unwrap();
    let view = client.targets().unwrap();
    let mut request = MaximizeRequest {
        workspace: 2,
        layout: "dwindle".into(),
        fullscreen: 0,
        action: MaximizeAction::Set,
        alt: false,
        policy_digest: policy.digest(),
    };
    match case {
        "changed_state" => state.lock().unwrap().fullscreen = 1,
        "lost_reply" => state.lock().unwrap().lose_ack = true,
        "policy_changed" => fs::write(
            &policy_path,
            r#"{"doubleTap":{"layouts":{"dwindle":"disabled"}}}"#,
        )
        .unwrap(),
        "policy_deleted" => fs::remove_file(&policy_path).unwrap(),
        "stale" => {
            invalidate_tx.send(()).unwrap();
            invalidated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        _ => {}
    }
    let result = client.maximize(&view, 0, &request);
    if case == "positive" {
        result.unwrap();
        assert_eq!(state.lock().unwrap().fullscreen, 1);
        request.fullscreen = 1;
        request.action = MaximizeAction::Unset;
        client.maximize(&view, 0, &request).unwrap();
        assert_eq!(state.lock().unwrap().fullscreen, 0);
        assert_eq!(state.lock().unwrap().submitted.len(), 2);
        client.release().unwrap();
    } else if case == "lost_reply" {
        assert_eq!(result, Err(modal_client::Error::EffectUnconfirmed));
        assert!(client.maximize(&view, 0, &request).is_err());
        assert_eq!(state.lock().unwrap().submitted.len(), 1);
    } else {
        assert!(result.is_err(), "{case}");
        assert!(state.lock().unwrap().submitted.is_empty(), "{case}");
    }
    drop(client);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    io.join().unwrap();
}
#[test]
fn actual_host_native_maximize_set_unset() {
    scenario("positive")
}
#[test]
fn actual_host_maximize_refuses_stale_and_foreign_role() {
    for case in ["stale", "foreign_role"] {
        scenario(case)
    }
}
#[test]
fn actual_host_maximize_refuses_disabled_custom_changed_or_omitted_policy() {
    for case in [
        "disabled",
        "custom",
        "omitted",
        "policy_changed",
        "policy_deleted",
    ] {
        scenario(case)
    }
}
#[test]
fn actual_host_maximize_changed_state_and_lost_reply() {
    for case in ["changed_state", "lost_reply"] {
        scenario(case)
    }
}
