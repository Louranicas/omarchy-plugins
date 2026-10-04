//! Full controlled fixture: authenticated desktop sockets -> app service ->
//! dedicated GTK child -> typed gesture -> existing arbiter -> simulated focus.
use desktop_io::{Endpoint, ProcessIdentity};
use modal_contract::Fence;
use modal_runtime::{
    actor::{Application, Effect, Error},
    backend::{Compositor, PresenterBackend, PresenterSpec},
    host::Host,
    readiness::Channel,
    targets::NativeTarget,
};
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use vimarchy_runtime::{native::Native, service::Service};
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
struct FixtureCompositor {
    root: PathBuf,
    channel: Option<Channel>,
    fence: Option<Fence>,
    focused: Arc<Mutex<Vec<String>>>,
}
impl Compositor for FixtureCompositor {
    fn prepare(&mut self, fence: Fence) -> Result<Vec<(OsString, OsString)>, Error> {
        let channel = Channel::bind(&self.root, fence).map_err(|_| Error::Unavailable)?;
        let env = channel.environment();
        self.channel = Some(channel);
        self.fence = Some(fence);
        Ok(env)
    }
    fn namespace(&self, fence: Fence) -> Option<String> {
        (self.fence == Some(fence))
            .then(|| self.channel.as_ref().map(|c| c.namespace().to_owned()))
            .flatten()
    }
    fn ready(&mut self, pid: u32, fence: Fence, deadline: Instant) -> Result<bool, Error> {
        if self.fence != Some(fence) {
            return Err(Error::Unavailable);
        }
        let channel = self.channel.as_mut().ok_or(Error::Unavailable)?;
        channel
            .receive(pid, deadline)
            .map_err(|_| Error::Unavailable)?;
        println!("fixture_child_map_authenticated=true pid={pid} compositor_layer_proof=simulated");
        channel.activate(deadline).map_err(|_| Error::Unavailable)?;
        Ok(true)
    }
    fn dismissed(&mut self, _: Fence, _: Instant) -> Result<bool, Error> {
        self.channel = None;
        self.fence = None;
        println!("fixture_layer_absence_proof=simulated");
        Ok(true)
    }
    fn dispatch(&mut self, _: Fence, effect: Effect, _: Instant) -> Result<(), Error> {
        println!("fixture_submap_effect={effect:?} native_dispatched=false");
        Ok(())
    }
    fn focus(&mut self, _: Fence, target: &NativeTarget, _: Instant) -> Result<(), Error> {
        self.focused.lock().unwrap().push(target.stable_id().into());
        println!(
            "fixture_focus_target={} native_dispatched=false",
            target.stable_id()
        );
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() != Ok("yes") {
        return Err("isolated display required".into());
    }
    let binary = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("presenter binary required")?;
    if !binary.is_absolute() {
        return Err("absolute presenter required".into());
    }
    let root = tempfile::tempdir()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
    for name in [
        "arbiter",
        "ready",
        "vimarchy-data",
        "ask-data",
        "yoohoo-data",
        "hypr",
        "hypr/fixture_1",
    ] {
        let p = root.path().join(name);
        std::fs::create_dir(&p)?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    let listener = UnixListener::bind(root.path().join("hypr/fixture_1/.socket.sock"))?;
    let event_listener = UnixListener::bind(root.path().join("hypr/fixture_1/.socket2.sock"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let event_stop = stop.clone();
    let events = thread::spawn(move || {
        let (peer, _) = event_listener.accept().unwrap();
        while !event_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        drop(peer);
    });
    let query_stop = stop.clone();
    let query = thread::spawn(move || {
        let clients=(0..3).map(|i|serde_json::json!({"address":format!("0x{}",i+1),"stableId":format!("{}",i+1),"mapped":true,"hidden":false,"workspace":{"id":1},"at":[80+i*300,100],"size":[240,180],"grouped":[],"focusHistoryID":i,"fullscreen":0,"class":"fixture-app","title":format!("Fixture window {}",i+1)})).collect::<Vec<_>>();
        let monitors = serde_json::json!([{"name":"HEADLESS-1","x":0,"y":0,"width":1280,"height":720,"scale":1,"activeWorkspace":{"id":1},"specialWorkspace":{"id":0}}]);
        listener.set_nonblocking(true).unwrap();
        while !query_stop.load(Ordering::Acquire) {
            let (mut peer, _) = match listener.accept() {
                Ok(peer) => peer,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            peer.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut request = [0; 128];
            let n = peer.read(&mut request).unwrap();
            let reply = match &request[..n] {
                b"j/clients" => serde_json::json!(clients),
                b"j/monitors" => monitors.clone(),
                b"j/activewindow" => serde_json::json!({"stableId":"1","address":"0x1"}),
                b"j/workspaces" => serde_json::json!([{"id":1,"tiledLayout":"dwindle"}]),
                other => panic!("unexpected query {other:?}"),
            };
            peer.write_all(&serde_json::to_vec(&reply).unwrap())
                .unwrap();
        }
    });
    let endpoint = Endpoint::discover_identity(root.path(), "fixture_1", identity())
        .map_err(|e| format!("endpoint {e:?}"))?;
    let mut native = Native::connect(endpoint, [7; 16]).map_err(|e| format!("native {e:?}"))?;
    let observation = native.observe().map_err(|e| format!("observe {e:?}"))?;
    let data = Service::bind(&root.path().join("vimarchy-data"))?;
    let focused = Arc::new(Mutex::new(Vec::new()));
    let environment = std::env::vars_os()
        .filter(|(name, _)| {
            [
                "XDG_RUNTIME_DIR",
                "WAYLAND_DISPLAY",
                "GDK_BACKEND",
                "GSK_RENDERER",
                "HOME",
                "PATH",
                "LD_LIBRARY_PATH",
                "OMARCHY_TEST_ISOLATED_DISPLAY",
            ]
            .iter()
            .any(|allowed| name == allowed)
        })
        .collect::<Vec<_>>();
    let specs = std::array::from_fn(|_| PresenterSpec {
        program: binary.clone(),
        arguments: vec![],
        environment: environment.clone(),
    });
    let compositor = FixtureCompositor {
        root: root.path().join("ready"),
        channel: None,
        fence: None,
        focused: focused.clone(),
    };
    let backend = PresenterBackend::new(compositor, specs).with_controller_endpoints([
        root.path().join("vimarchy-data/modal.sock"),
        root.path().join("ask-data/modal.sock"),
        root.path().join("yoohoo-data/modal.sock"),
    ]);
    let mut host = Host::bind(&root.path().join("arbiter"), backend, |_, pid| {
        (pid as u32 == std::process::id()).then_some(Application::Vimarchy)
    })?;
    let source = host.begin_source().map_err(|e| format!("source {e:?}"))?;
    host.snapshot(
        source,
        1,
        (1..=3)
            .map(|i| NativeTarget::new("fixture_1", &i.to_string()).unwrap())
            .collect(),
    )
    .map_err(|e| format!("snapshot {e:?}"))?;
    let host_stop = stop.clone();
    let host_thread = thread::spawn(move || {
        while !host_stop.load(Ordering::Acquire) {
            if let Err(e) = host.step() {
                println!("fixture_host_stop={e}");
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let result = (|| -> Result<(), String> {
        let mut client = modal_client::Client::connect(&root.path().join("arbiter"), identity())
            .map_err(|e| format!("client {e:?}"))?;
        let lease = client
            .acquire(Duration::from_secs(5))
            .map_err(|e| format!("acquire {e:?}"))?;
        let presenter = lease.presenter.ok_or("missing presenter receipt")?;
        println!(
            "fixture_presenter_receipt_verified=true pid={} namespace={}",
            presenter.process().pid,
            presenter.namespace()
        );
        let mut service = Service::attach(data, client, native, observation)
            .map_err(|e| format!("attach {e}"))?;
        let end = Instant::now() + Duration::from_secs(4);
        while !service.is_closed() && Instant::now() < end {
            if let Err(e) = service.step() {
                println!("fixture_service_stop={e}");
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        service.close();
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    host_thread.join().map_err(|_| "host panic")?;
    query.join().map_err(|_| "query panic")?;
    events.join().map_err(|_| "events panic")?;
    result.map_err(|e| format!("fixture failed: {e}"))?;
    let calls = focused.lock().unwrap().clone();
    println!(
        "fixture_end_to_end_focus_count={} targets={calls:?} live_desktop_changed=false",
        calls.len()
    );
    if calls != ["1"] {
        return Err("expected one fixture focus".into());
    }
    Ok(())
}
