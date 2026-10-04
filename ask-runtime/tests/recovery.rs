//! Synthetic configured adapters only. No vendor authentication or modal grant.
use ask_core::{Id, RequestId, Text, session::Phase};
use ask_runtime::{Error, Event, Instance, Worker, provider::ProviderProfile};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

struct Clock {
    origin: Instant,
    offset: u64,
}
impl Clock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            offset: 0,
        }
    }
    fn now(&self) -> u64 {
        self.offset + self.origin.elapsed().as_millis() as u64
    }
    fn advance(&mut self, ms: u64) {
        self.offset += ms;
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    profile: ProviderProfile,
    receipt: std::path::PathBuf,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("adapter.py");
        let receipt = root.path().join("receipt.jsonl");
        fs::write(&script, include_str!("fixtures/recovery.py")).unwrap();
        let value = json!({"version":1,"harness":"codex","adapter_argv":["/usr/bin/python3",script,receipt,mode],"harness_executable":"/usr/bin/python3","cwd":root.path()});
        let profile_path = root.path().join("profile.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&profile_path)
            .unwrap();
        file.write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        drop(file);
        let profile = ProviderProfile::read(&profile_path).unwrap();
        Self {
            _root: root,
            profile,
            receipt,
        }
    }
    fn spawn(&self, clock: &Clock) -> Worker {
        self.profile.preflight().unwrap().recheck().unwrap();
        Worker::spawn(
            self.profile.launch().clone(),
            self.profile.environment(|_| None).unwrap(),
            clock.now(),
        )
        .unwrap()
    }
    fn records(&self) -> Vec<Value> {
        // A concurrently appended final line may be incomplete; only complete
        // records witness progress. Final assertions require their exact values.
        fs::read_to_string(&self.receipt)
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
fn until(
    worker: &mut Worker,
    clock: &Clock,
    predicate: impl Fn(&Worker, &[Event]) -> bool,
) -> Vec<Event> {
    let end = Instant::now() + Duration::from_secs(2);
    let mut seen = vec![];
    loop {
        seen.extend(worker.pump(clock.now(), &AtomicBool::new(false)).unwrap());
        if predicate(worker, &seen) {
            return seen;
        }
        assert!(
            Instant::now() < end,
            "fixture did not progress: {:?} {seen:?}",
            worker.adapter().session().phase()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[derive(Clone)]
struct Permission {
    instance: Instance,
    generation: u64,
    epoch: u64,
    request: RequestId,
}
fn submit(worker: &mut Worker, clock: &Clock, turn: &str) -> Permission {
    worker
        .adapter_mut()
        .submit(
            Id::new(turn).unwrap(),
            Text::new("synthetic request").unwrap(),
            clock.now(),
        )
        .unwrap();
    let events = until(worker, clock, |_, events| {
        events.iter().any(|e| matches!(e, Event::Permission { .. }))
    });
    events
        .into_iter()
        .find_map(|event| match event {
            Event::Permission {
                instance,
                generation,
                epoch,
                request,
                ..
            } => Some(Permission {
                instance,
                generation,
                epoch,
                request,
            }),
            _ => None,
        })
        .unwrap()
}
fn answer(worker: &mut Worker, clock: &Clock, permission: &Permission) -> Result<(), Error> {
    worker.adapter_mut().answer(
        permission.instance,
        permission.generation,
        permission.epoch,
        &permission.request,
        Some(&Id::new("allow").unwrap()),
        clock.now(),
    )
}
fn close(worker: &mut Worker, clock: &Clock) {
    worker.adapter_mut().close(clock.now()).unwrap();
    until(worker, clock, |w, _| {
        w.reap_receipt().state() == acp_transport::ReapState::Reaped
    });
}
fn start_ticks(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}
#[test]
fn configured_cooperative_cancel_drains_late_permission_then_next_turn_is_fresh() {
    let fixture = Fixture::new("cooperative");
    let clock = Clock::new();
    let mut worker = fixture.spawn(&clock);
    until(&mut worker, &clock, |w, _| {
        w.adapter().session().phase() == Phase::Ready
    });
    let old = submit(&mut worker, &clock, "first");
    worker.adapter_mut().cancel(clock.now()).unwrap();
    until(&mut worker, &clock, |w, _| {
        w.adapter().session().active_turn().is_none()
            && fixture.records().iter().any(|r| r["received"]["id"] == 78)
    });
    assert_eq!(worker.adapter().session().permissions().pending_count(), 0);
    assert!(
        !worker
            .adapter()
            .session()
            .messages()
            .iter()
            .any(|m| m.body.as_str().contains("cancelled late text"))
    );
    let records = fixture.records();
    let denied = records
        .iter()
        .position(|r| {
            r["received"]["id"] == 77
                && r["received"]["result"]["outcome"]["outcome"] == "cancelled"
        })
        .unwrap();
    let cancelled = records
        .iter()
        .position(|r| r["received"]["method"] == "session/cancel")
        .unwrap();
    assert!(denied < cancelled);
    assert!(records.iter().any(|r| r["received"]["id"] == 78
        && r["received"]["result"]["outcome"]["outcome"] == "cancelled"));
    let next = submit(&mut worker, &clock, "next");
    assert_eq!(old.instance, next.instance);
    assert_eq!(old.request, next.request);
    assert_ne!(old.epoch, next.epoch);
    assert!(answer(&mut worker, &clock, &old).is_err());
    assert_eq!(worker.adapter().session().permissions().pending_count(), 1);
    answer(&mut worker, &clock, &next).unwrap();
    until(&mut worker, &clock, |w, _| {
        w.adapter().session().active_turn().is_none()
    });
    close(&mut worker, &clock);
}
#[test]
fn ignored_cancel_escalates_to_actual_reap_before_explicit_fresh_replacement() {
    let fixture = Fixture::new("unresponsive");
    let mut clock = Clock::new();
    let mut old_worker = fixture.spawn(&clock);
    until(&mut old_worker, &clock, |w, _| {
        w.adapter().session().phase() == Phase::Ready
    });
    let unrelated_fixture = Fixture::new("cooperative");
    let mut unrelated = unrelated_fixture.spawn(&clock);
    until(&mut unrelated, &clock, |w, _| {
        w.adapter().session().phase() == Phase::Ready
    });
    let unrelated_pid = unrelated_fixture
        .records()
        .iter()
        .find_map(|r| r["pid"].as_u64())
        .unwrap() as u32;
    let unrelated_identity = start_ticks(unrelated_pid).expect("witness unrelated live child");
    let old = submit(&mut old_worker, &clock, "first");
    let pid = fixture
        .records()
        .iter()
        .find_map(|r| r["pid"].as_u64())
        .unwrap() as u32;
    let identity = start_ticks(pid).expect("witness actual admitted child");
    old_worker.adapter_mut().cancel(clock.now()).unwrap();
    until(&mut old_worker, &clock, |_, _| {
        fixture
            .records()
            .iter()
            .any(|r| r["received"]["method"] == "session/cancel")
    });
    assert_eq!(
        old_worker.reap_receipt().state(),
        acp_transport::ReapState::Running
    );
    clock.advance(5001);
    until(&mut old_worker, &clock, |_, _| {
        fixture
            .records()
            .iter()
            .any(|r| r["received"]["method"] == "session/close")
    });
    clock.advance(351);
    until(&mut old_worker, &clock, |_, _| {
        fixture.records().iter().any(|r| r["signal"] == "term")
    });
    assert_eq!(
        start_ticks(pid),
        Some(identity),
        "TERM-ignoring child already disappeared"
    );
    clock.advance(501);
    until(&mut old_worker, &clock, |w, _| {
        w.reap_receipt().state() == acp_transport::ReapState::Reaped
    });
    assert_ne!(start_ticks(pid), Some(identity));
    assert!(answer(&mut old_worker, &clock, &old).is_err());
    drop(old_worker);
    assert_eq!(start_ticks(unrelated_pid), Some(unrelated_identity));
    let survivor = submit(&mut unrelated, &clock, "survived-other-cancellation");
    answer(&mut unrelated, &clock, &survivor).unwrap();
    until(&mut unrelated, &clock, |w, _| {
        w.adapter().session().active_turn().is_none()
    });
    close(&mut unrelated, &clock);
    // Explicit fresh construction after actual reap; not UI restart, retained
    // modal admission, transcript replay, or a claim of generation increment.
    let mut worker = fixture.spawn(&clock);
    until(&mut worker, &clock, |w, _| {
        w.adapter().session().phase() == Phase::Ready
    });
    let next = submit(&mut worker, &clock, "first");
    assert_ne!(old.instance, next.instance);
    assert_eq!(old.generation, next.generation);
    assert_eq!(old.epoch, next.epoch);
    assert_eq!(old.request, next.request);
    assert_eq!(answer(&mut worker, &clock, &old), Err(Error::Stale));
    assert_eq!(worker.adapter().session().permissions().pending_count(), 1);
    answer(&mut worker, &clock, &next).unwrap();
    until(&mut worker, &clock, |w, _| {
        w.adapter().session().active_turn().is_none()
    });
    close(&mut worker, &clock);
}
