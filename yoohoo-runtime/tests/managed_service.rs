//! Existing Host + app service with an explicit simulated lifecycle, not GTK proof.
use desktop_io::ProcessIdentity;
use modal_contract::{Fence, Owner};
use modal_runtime::{
    actor::{Application, Backend, Dismissal, Effect, Error},
    backend::PresenterIdentity,
    host::Host,
    targets::NativeTarget,
};
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use yoohoo_runtime::{Command, Runtime, native::Native, presenter::Link, service::Service};
fn identity() -> ProcessIdentity {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    ProcessIdentity {
        pid: std::process::id(),
        start_ticks: s
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
struct Simulated(Arc<Mutex<Vec<String>>>);
impl Backend for Simulated {
    fn register(&mut self, _: Owner, app: Application) -> Result<(), Error> {
        assert_eq!(app, Application::Yoohoo);
        Ok(())
    }
    fn presenter_identity(&mut self, _: Fence) -> Result<Option<PresenterIdentity>, Error> {
        let id = identity();
        Ok(Some(PresenterIdentity {
            pid: id.pid,
            start_ticks: id.start_ticks.to_string(),
            namespace: format!("omarchy-modal-{}", "a".repeat(32)),
        }))
    }
    fn present(&mut self, _: Fence, _: u64) -> Result<(), Error> {
        Ok(())
    }
    fn dismiss(&mut self, _: Fence) -> Result<Dismissal, Error> {
        Ok(Dismissal {
            presenter_exited: true,
            compositor_dismissed: true,
        })
    }
    fn dispatch(&mut self, _: Fence, _: u64, effect: Effect) -> Result<(), Error> {
        assert_eq!(effect, Effect::EnterYoohoo);
        Ok(())
    }
    fn focus(&mut self, _: Fence, _: u64, target: &NativeTarget) -> Result<(), Error> {
        self.0.lock().unwrap().push(target.stable_id().into());
        Ok(())
    }
}
fn scenario(stale_registry: bool, source_loss: bool, events_mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["arbiter", "data", "hypr", "hypr/test"] {
        let p = dir.path().join(name);
        std::fs::create_dir(&p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = UnixListener::bind(dir.path().join("hypr/test/.socket.sock")).unwrap();
    let event = UnixListener::bind(dir.path().join("hypr/test/.socket2.sock")).unwrap();
    let queries = thread::spawn(move || {
        for reply in [
            r#"[{"address":"0x1","stableId":"000A","title":"Attention 日本語","class":"Terminal","workspace":{"id":1,"name":"One"}}]"#,
            "{}",
        ] {
            let (mut peer, _) = listener.accept().unwrap();
            let mut bytes = [0; 64];
            assert!(peer.read(&mut bytes).unwrap() > 0);
            peer.write_all(reply.as_bytes()).unwrap();
        }
        listener
    });
    let origin = Instant::now();
    let endpoint = desktop_io::Endpoint::discover_identity(dir.path(), "test", identity()).unwrap();
    let mut native = Native::connect(endpoint, [7; 16]).unwrap();
    let (mut events, _) = event.accept().unwrap();
    let mut runtime = Runtime::empty().unwrap();
    native.synchronize(&mut runtime, 0).unwrap();
    let listener = queries.join().unwrap();
    events.write_all(b"urgent>>0x1\n").unwrap();
    native
        .poll(&mut runtime, origin.elapsed().as_millis() as u64)
        .unwrap();
    assert_eq!(runtime.view().rows.len(), 1);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut host = Host::bind(
        &dir.path().join("arbiter"),
        Simulated(calls.clone()),
        |_, pid| (pid as u32 == std::process::id()).then_some(Application::Yoohoo),
    )
    .unwrap();
    let source = host.begin_source().unwrap();
    host.snapshot(source, 1, vec![NativeTarget::new("test", "a").unwrap()])
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let invalidate = Arc::new(AtomicBool::new(false));
    let invalidated = Arc::new(AtomicBool::new(false));
    let hs = stop.clone();
    let hi = invalidate.clone();
    let ha = invalidated.clone();
    let host_thread = thread::spawn(move || {
        while !hs.load(Ordering::Acquire) {
            if hi.load(Ordering::Acquire) && !ha.load(Ordering::Acquire) {
                let s = host.begin_source().unwrap();
                host.snapshot(s, 1, vec![NativeTarget::new("test", "a").unwrap()])
                    .unwrap();
                ha.store(true, Ordering::Release);
            }
            host.step().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });
    let socket = Service::bind(&dir.path().join("data")).unwrap();
    let mut client =
        modal_client::Client::connect(&dir.path().join("arbiter"), identity()).unwrap();
    let lease = client.acquire(Duration::from_secs(5)).unwrap();
    let auth =
        modal_client::presenter::Auth::new(lease.fence, lease.presenter.unwrap().namespace())
            .unwrap();
    let mut service = Service::attach(socket, client, native, runtime, origin).unwrap();
    let service_thread = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(2);
        while !service.is_closed() && Instant::now() < end {
            if service.step().is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        service.close();
    });
    let mut link = Link::connect(&dir.path().join("data/modal.sock"), identity(), auth).unwrap();
    let mut model = link.model().unwrap();
    assert_eq!(model.view.rows.len(), 1);
    assert!(calls.lock().unwrap().is_empty(), "opening must not focus");
    let mut stale_event = false;
    let mut cleared = false;
    if !events_mode.is_empty() {
        let original = model.view.clone();
        events.write_all(b"notification>>untrusted\n").unwrap();
        model = link.model().unwrap();
        assert_eq!(
            model.view, original,
            "ignored events do not close or revise model"
        );
        events.write_all(b"urgent>>0x1\n").unwrap();
        if events_mode == "stale" {
            stale_event = true;
        } else {
            model = link.model().unwrap();
            assert_eq!(model.view.rows[0].id, original.rows[0].id);
            assert_eq!(model.view.rows[0].count, 2);
            assert!(!model.view.pending_activation);
            if events_mode == "metadata" {
                let reply = thread::spawn(move || {
                    let (mut peer, _) = listener.accept().unwrap();
                    let mut bytes = [0; 64];
                    assert!(peer.read(&mut bytes).unwrap() > 0);
                    peer.write_all(r#"[{"address":"0x1","stableId":"a","title":"New title 日本語","class":"Terminal","workspace":{"id":2,"name":"Two"}}]"#.as_bytes()).unwrap();
                });
                events.write_all("windowtitle>>0x1\nwindowtitlev2>>0x1,New title 日本語\nmovewindowv2>>0x1,2,Two\n".as_bytes()).unwrap();
                model = link.model().unwrap();
                reply.join().unwrap();
                assert_eq!(model.view.rows[0].title, "New title 日本語");
                assert_eq!(model.view.rows[0].workspace, "Two");
                assert_eq!(model.view.rows[0].id, original.rows[0].id);
                assert_eq!(model.view.rows[0].count, 2);
                assert!(model.view.rows[0].selected);
                assert!(!model.view.pending_activation);
            } else if events_mode == "focus" || events_mode == "close" {
                events
                    .write_all(if events_mode == "focus" {
                        b"activewindowv2>>0x1\n"
                    } else {
                        b"closewindow>>0x1\n"
                    })
                    .unwrap();
                model = link.model().unwrap();
                assert!(
                    model.view.open && model.view.rows.is_empty() && !model.view.pending_activation
                );
                cleared = true;
            }
        }
    }
    if stale_registry {
        invalidate.store(true, Ordering::Release);
        let end = Instant::now() + Duration::from_secs(1);
        while !invalidated.load(Ordering::Acquire) {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(1));
        }
    }
    if source_loss {
        drop(events);
    }
    let result = link.apply(if cleared {
        Command::Close {
            generation: model.view.generation,
        }
    } else {
        Command::Activate {
            generation: model.view.generation,
            revision: model.view.revision,
            id: model.view.rows[0].id,
        }
    });
    if stale_registry || source_loss || stale_event {
        assert!(result.is_err());
    } else {
        assert!(result.unwrap());
    }
    drop(link);
    service_thread.join().unwrap();
    stop.store(true, Ordering::Release);
    host_thread.join().unwrap();
    assert_eq!(
        calls.lock().unwrap().len(),
        usize::from(!stale_registry && !source_loss && !stale_event && !cleared)
    );
}
#[test]
fn native_attention_display_and_typed_focus() {
    scenario(false, false, "")
}
#[test]
fn arbiter_resnapshot_rejects_old_display_token() {
    scenario(true, false, "")
}
#[test]
fn native_stream_loss_rejects_pending_activation() {
    scenario(false, true, "")
}

#[test]
fn metadata_and_attention_refresh_keep_managed_list_and_identity() {
    scenario(false, false, "metadata");
}
#[test]
fn focus_clears_row_without_closing_managed_list() {
    scenario(false, false, "focus");
}
#[test]
fn close_clears_row_without_closing_managed_list() {
    scenario(false, false, "close");
}
#[test]
fn native_update_rejects_preupdate_activation_without_focus() {
    scenario(false, false, "stale");
}
