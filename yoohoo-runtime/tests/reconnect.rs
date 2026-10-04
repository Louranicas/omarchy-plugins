//! Actual Unix source disconnects; simulated compositor data, no modal authority.
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
