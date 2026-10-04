//! Real client/Host/PresenterBackend/native IPC. Readiness and compositor are simulated.
use desktop_io::{DispatchDialect, ProcessIdentity};
use modal_client::Client;
use modal_contract::Fence;
use modal_runtime::{
    actor::{Application, Effect, Error},
    backend::{Compositor, PresenterBackend, PresenterSpec},
    configured::Config,
    host::Host,
    native::{NativeCompositor, NativeFocus, SUPPORTED_COMMIT},
    workspace_move::{MoveWorkspaceRequest, WorkspaceIdentity},
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
struct State {
    source: i64,
    active: i64,
    destination_monitor: i64,
    destination_name: String,
    target: String,
    grouped: bool,
    pinned: bool,
    swallowing: bool,
    missing_destination: bool,
    submitted: Vec<String>,
    lose_ack: bool,
    bad_readback: bool,
    foreign_event: bool,
    wrong_follow: bool,
    focus_reads: usize,
    lose_final_focus: bool,
}
fn workspace(id: i64) -> WorkspaceIdentity {
    WorkspaceIdentity {
        id,
        name: id.to_string(),
        monitor_id: 0,
    }
}
fn scenario(case: &str) {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let native = temp.path().join("native");
    let modal = temp.path().join("modal");
    for name in [
        "native",
        "modal",
        "vimarchy-data",
        "ask-data",
        "yoohoo-data",
    ] {
        let path = temp.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir_all(native.join("hypr/fixture")).unwrap();
    let commands = UnixListener::bind(native.join("hypr/fixture/.socket.sock")).unwrap();
    commands.set_nonblocking(true).unwrap();
    let events = UnixListener::bind(native.join("hypr/fixture/.socket2.sock")).unwrap();
    events.set_nonblocking(true).unwrap();
    let state = Arc::new(Mutex::new(State {
        source: 2,
        active: 2,
        destination_monitor: 0,
        destination_name: "3".into(),
        target: "ab".into(),
        grouped: false,
        pinned: false,
        swallowing: false,
        missing_destination: false,
        submitted: Vec::new(),
        lose_ack: false,
        bad_readback: false,
        foreign_event: false,
        wrong_follow: false,
        focus_reads: 0,
        lose_final_focus: false,
    }));
    let observed = state.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let io = thread::spawn(move || {
        let mut peers = Vec::new();
        while !stopped.load(Ordering::Acquire) {
            if let Ok((stream, _)) = events.accept() {
                peers.push(stream)
            }
            if let Ok((mut stream, _)) = commands.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut b = [0; 2048];
                let n = stream.read(&mut b).unwrap();
                let wire = std::str::from_utf8(&b[..n]).unwrap();
                let mut s = observed.lock().unwrap();
                let reply=match wire {
    "j/version"=>json!({"commit":SUPPORTED_COMMIT,"dirty":false}).to_string(),
    "j/clients"=>json!([{"stableId":s.target,"address":"0x1","mapped":true,"hidden":false,"pinned":s.pinned,"swallowing":if s.swallowing{"0x2"}else{"0x0"},"grouped":if s.grouped{vec!["0x1","0x2"]}else{vec![]},"fullscreen":0,"monitor":0,"workspace":{"id":s.source,"name":s.source.to_string()}}]).to_string(),
    "j/workspaces"=>{
      let mut rows=vec![json!({"id":1,"name":"1","monitorID":0}),json!({"id":2,"name":"2","monitorID":0})];
      if !s.missing_destination {rows.push(json!({"id":3,"name":s.destination_name,"monitorID":s.destination_monitor}));}json!(rows).to_string()
    },
    "j/activeworkspace"=>json!({"id":s.active,"name":s.active.to_string(),"monitorID":0}).to_string(),
    "j/activewindow"=>{s.focus_reads+=1;if s.source==s.active && !(s.lose_final_focus && s.focus_reads>1) {json!({"stableId":"ab","address":"0x1","class":"app","title":"fixture"}).to_string()}else{"{}".into()}},
    wire if wire.starts_with("/dispatch hl.dsp.window.move(")=>{
      assert!(wire=="/dispatch hl.dsp.window.move({workspace=\"3\",follow=true,window=\"stableid:ab\"})" || wire=="/dispatch hl.dsp.window.move({workspace=\"3\",follow=false,window=\"stableid:ab\"})");
      s.submitted.push(wire.into());let follow=wire.contains("follow=true");if !s.bad_readback{s.source=3;}
      if follow!=s.wrong_follow{s.active=3;}
      let address=if s.foreign_event{"f"}else{"1"};
      for peer in &mut peers {peer.write_all(format!("movewindow>>{address},3\nmovewindowv2>>{address},3,3\n").as_bytes()).unwrap();if follow{peer.write_all(b"workspace>>3\nworkspacev2>>3,3\nactivewindow>>app,fixture\nactivewindowv2>>1\n").unwrap();}}
      if s.lose_ack {String::new()}else{"ok".into()}
    },
    other=>panic!("unexpected wire {other}"),
   };
                stream.write_all(reply.as_bytes()).unwrap();
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let process = modal_runtime::process_identity::Pinned::capture(std::process::id())
        .unwrap()
        .identity();
    let mut config = json!({"version":1,"vimarchy_numeric_workspace_moves":case!="disabled","runtime":modal,"compositor":{"runtime":native,"instance":"fixture","process":process,"dialect":"lua"},"controllers":[{"role":"vimarchy","process":process}],"presenters":(["vimarchy","ask","yoohoo"].map(|role|json!({"role":role,"program":"/usr/bin/sleep","arguments":["20"],"environment":[],"controller_socket":temp.path().join(format!("{role}-data/modal.sock"))})))});
    if case == "omitted" {
        config
            .as_object_mut()
            .unwrap()
            .remove("vimarchy_numeric_workspace_moves");
    }
    let config_path = temp.path().join("host.json");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    let (_, stamp) = Config::read_with_digest(&config_path).unwrap();
    let admission = if case == "omitted" {
        None
    } else {
        Some((config_path.clone(), stamp))
    };
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
        .with_workspace_move_config(admission);
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
    let mut request = MoveWorkspaceRequest {
        source: workspace(2),
        destination: workspace(3),
        active: workspace(2),
        follow: case != "silent" && case != "wrong_silent",
    };
    match case {
        "source_changed" => state.lock().unwrap().source = 1,
        "destination_changed" => state.lock().unwrap().destination_name = "renamed".into(),
        "monitor_changed" => state.lock().unwrap().destination_monitor = 1,
        "active_changed" => state.lock().unwrap().active = 1,
        "target_reused" => state.lock().unwrap().target = "ac".into(),
        "grouped" => state.lock().unwrap().grouped = true,
        "pinned" => state.lock().unwrap().pinned = true,
        "swallowing" => state.lock().unwrap().swallowing = true,
        "missing_destination" => state.lock().unwrap().missing_destination = true,
        "final_focus_lost" => state.lock().unwrap().lose_final_focus = true,
        "noop_other_active" => {
            request.destination = workspace(2);
            request.active = workspace(1);
            state.lock().unwrap().active = 1;
        }
        "lost_ack" => state.lock().unwrap().lose_ack = true,
        "bad_readback" => state.lock().unwrap().bad_readback = true,
        "foreign_event" => state.lock().unwrap().foreign_event = true,
        "wrong_silent" | "wrong_follow" => state.lock().unwrap().wrong_follow = true,
        "revoked" => {
            config["vimarchy_numeric_workspace_moves"] = false.into();
            fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        "changed_authority" => {
            config["controllers"][0]["role"] = "ask".into();
            fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        "deleted_config" => fs::remove_file(&config_path).unwrap(),
        "malformed_config" => fs::write(&config_path, b"{}").unwrap(),
        "stale" => {
            invalidate_tx.send(()).unwrap();
            invalidated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        "noop" => request.destination = workspace(2),
        "scratchpad" => request.destination.id = -1,
        _ => {}
    }
    let result = client.move_workspace(&view, 0, &request);
    if matches!(case, "follow" | "silent" | "noop" | "noop_other_active") {
        result.unwrap();
        let s = state.lock().unwrap();
        assert_eq!(
            s.submitted.len(),
            if case.starts_with("noop") { 0 } else { 1 }
        );
        assert_eq!(s.source, if case.starts_with("noop") { 2 } else { 3 });
        assert_eq!(
            s.active,
            if case == "follow" {
                3
            } else if case == "noop_other_active" {
                1
            } else {
                2
            }
        );
        drop(s);
        client.release().unwrap();
    } else if matches!(
        case,
        "final_focus_lost"
            | "lost_ack"
            | "bad_readback"
            | "foreign_event"
            | "wrong_follow"
            | "wrong_silent"
    ) {
        assert_eq!(
            result,
            Err(modal_client::Error::EffectUnconfirmed),
            "{case}"
        );
        assert!(client.move_workspace(&view, 0, &request).is_err());
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
fn workspace_follow_silent_and_verified_noop() {
    for case in ["follow", "silent", "noop", "noop_other_active"] {
        scenario(case)
    }
}
#[test]
fn workspace_full_authority_and_live_config() {
    for case in [
        "omitted",
        "disabled",
        "revoked",
        "deleted_config",
        "malformed_config",
        "changed_authority",
        "foreign_role",
        "stale",
    ] {
        scenario(case)
    }
}
#[test]
fn workspace_source_destination_monitor_and_target_changes_never_submit() {
    for case in [
        "source_changed",
        "destination_changed",
        "monitor_changed",
        "active_changed",
        "target_reused",
        "missing_destination",
    ] {
        scenario(case)
    }
}
#[test]
fn workspace_unsupported_targets_and_scratchpad_never_submit() {
    for case in ["grouped", "pinned", "swallowing", "scratchpad"] {
        scenario(case)
    }
}
#[test]
fn workspace_lost_ack_bad_readback_wrong_follow_and_foreign_event_latch_uncertainty() {
    for case in [
        "final_focus_lost",
        "lost_ack",
        "bad_readback",
        "wrong_follow",
        "wrong_silent",
        "foreign_event",
    ] {
        scenario(case)
    }
}
