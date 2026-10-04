//! Actual Linux process metadata, synthetic vendor comm labels: no provider acceptance.
use agentd::{
    SystemClock,
    model::{Harness, ScanState, Snapshot},
    procfs::{ProcfsScanner, parse_stat},
    state::StateStore,
};
use omarchy_process_runner::{CleanupReceipt, CleanupState, ReapReservation, Request, run};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
const BUDGET: Duration = Duration::from_secs(3);
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "agentd-owned-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild {
    child: Option<Child>,
    reservation: Option<ReapReservation>,
}
impl OwnedChild {
    fn spawn(mut command: Command) -> Self {
        let reservation = ReapReservation::reserve().unwrap();
        let child = command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            reservation: Some(reservation),
        }
    }
    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    fn graceful(&mut self) {
        assert!(self.child.as_mut().unwrap().try_wait().unwrap().is_none());
        assert_eq!(unsafe { libc::kill(self.pid() as i32, libc::SIGTERM) }, 0);
        let end = Instant::now() + BUDGET;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                assert!(status.success(), "daemon SIGTERM exit status: {status}");
                self.child.take();
                self.reservation.take();
                return;
            }
            assert!(Instant::now() < end, "daemon SIGTERM cleanup deadline");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn stop(&mut self) -> CleanupReceipt {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        self.reservation.take().unwrap().handoff(child, ())
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}
fn reap(receipt: &CleanupReceipt) {
    let end = Instant::now() + BUDGET;
    while receipt.state() != CleanupState::Reaped {
        assert!(
            Instant::now() < end,
            "owned fixture child cleanup deadline: {:?}",
            receipt.state()
        );
        thread::sleep(Duration::from_millis(5));
    }
}
struct Capture {
    children: Vec<OwnedChild>,
    root: Directory,
    cwd: Directory,
}
impl Capture {
    fn new() -> Self {
        let root = Directory::new();
        let cwd = Directory::new();
        symlink("/bin/sleep", root.0.join("codex")).unwrap();
        symlink("/bin/sleep", root.0.join("claude")).unwrap();
        let mut children = Vec::new();
        for label in ["codex", "codex", "codex", "claude"] {
            let mut command = Command::new(root.0.join(label));
            command.arg("30").current_dir(&cwd.0);
            let child = OwnedChild::spawn(command);
            let pid = child.pid();
            children.push(child); // retain custody before any assertion/read can fail
            let proc = PathBuf::from(format!("/proc/{pid}"));
            let end = Instant::now() + BUDGET;
            let stat = loop {
                let text = fs::read_to_string(proc.join("stat")).unwrap();
                if parse_stat(&text, pid).unwrap().comm == label {
                    break text;
                }
                assert!(Instant::now() < end, "owned process did not finish exec");
                thread::sleep(Duration::from_millis(2));
            };
            let dir = root.0.join(pid.to_string());
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("stat"), &stat).unwrap();
            fs::write(dir.join("status"), fs::read(proc.join("status")).unwrap()).unwrap();
            symlink(fs::read_link(proc.join("cwd")).unwrap(), dir.join("cwd")).unwrap();
            assert_eq!(
                parse_stat(&fs::read_to_string(proc.join("stat")).unwrap(), pid)
                    .unwrap()
                    .start_time_ticks,
                parse_stat(&stat, pid).unwrap().start_time_ticks
            );
        }
        // Preserve the complete actual ancestry needed by root-collapse validation.
        // Ancestors contribute stat/status/cwd only; no arguments, environment or terminal data.
        let mut parent = std::process::id();
        let mut visited = std::collections::BTreeSet::new();
        while parent != 0 {
            assert!(visited.len() < 256 && visited.insert(parent));
            let stat = fs::read_to_string(format!("/proc/{parent}/stat")).unwrap();
            let dir = root.0.join(parent.to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("stat"), &stat).unwrap();
            fs::write(
                dir.join("status"),
                fs::read(format!("/proc/{parent}/status")).unwrap(),
            )
            .unwrap();
            if let Ok(cwd) = fs::read_link(format!("/proc/{parent}/cwd")) {
                symlink(cwd, dir.join("cwd")).unwrap();
            }
            parent = parse_stat(&stat, parent).unwrap().parent_pid;
        }
        // Only the nonsensitive boot-time input, not unrelated system counters.
        let system = fs::read_to_string("/proc/stat").unwrap();
        fs::write(
            root.0.join("stat"),
            system.lines().find(|x| x.starts_with("btime ")).unwrap(),
        )
        .unwrap();
        Self {
            children,
            root,
            cwd,
        }
    }
    fn scanner(&self) -> ProcfsScanner {
        ProcfsScanner::new(self.root.0.clone(), unsafe { libc::geteuid() })
    }
    fn pids(&self) -> Vec<u32> {
        self.children.iter().map(OwnedChild::pid).collect()
    }
}
fn cli(runtime: &Path, args: &[&str]) -> Snapshot {
    let out = run(
        &Request {
            program: env!("CARGO_BIN_EXE_agentd").into(),
            arguments: args.iter().map(Into::into).collect(),
            directory: runtime.into(),
            environment: vec![("XDG_RUNTIME_DIR".into(), runtime.as_os_str().into())],
            timeout: BUDGET,
            output_limit: 1024 * 1024,
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "isolated CLI failed: {:?}",
        out.stderr
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn owned_capture_replay_matches_daemon_cli_and_observes_exit() {
    let mut capture = Capture::new();
    let pids = capture.pids();
    let proposal = capture.scanner().scan(None, &SystemClock);
    let live = ProcfsScanner::system();
    let roots: std::collections::BTreeSet<_> = pids
        .iter()
        .enumerate()
        .map(|(i, pid)| {
            live.resolve_hook_root(
                *pid,
                if i == 3 {
                    Harness::Claude
                } else {
                    Harness::Codex
                },
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        proposal.agents.len(),
        roots.len(),
        "capture must preserve actual ancestor collapse"
    );
    assert!(proposal.agents.iter().all(|a| roots.contains(&a.id)
        && a.id.start_time_ticks > 0
        && a.started_at_unix_ms.is_some()));
    assert!(
        proposal
            .agents
            .iter()
            .filter(|a| pids.contains(&a.id.pid))
            .all(|a| a.cwd.value.as_deref() == capture.cwd.0.to_str())
    );
    let runtime = Directory::new();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    // env_clear is applied at spawn; explicitly supplied environment is added below.
    cmd.arg("daemon").current_dir(&runtime.0);
    let reservation = ReapReservation::reserve().unwrap();
    let child = cmd
        .env_clear()
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .env("XDG_STATE_HOME", runtime.0.join("state"))
        .env("PATH", runtime.0.join("no-tools"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = OwnedChild {
        child: Some(child),
        reservation: Some(reservation),
    };
    let end = Instant::now() + BUDGET;
    while !runtime.0.join("agentd.sock").exists() {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(5));
    }
    let end = Instant::now() + BUDGET;
    loop {
        let snapshot = cli(&runtime.0, &["list", "--json"]);
        let found: Vec<_> = snapshot
            .agents
            .iter()
            .filter(|a| roots.contains(&a.id))
            .collect();
        if found.len() == roots.len() {
            for expected in &proposal.agents {
                assert!(found.iter().any(|a| a.id == expected.id
                    && a.harness == expected.harness
                    && a.cwd == expected.cwd));
            }
            break;
        }
        assert!(
            Instant::now() < end,
            "daemon did not observe ancestry-resolved process roots"
        );
        thread::sleep(Duration::from_millis(10));
    }
    for child in &mut capture.children {
        reap(&child.stop());
    }
    let end = Instant::now() + BUDGET;
    loop {
        if cli(&runtime.0, &["list", "--json"])
            .agents
            .iter()
            .all(|a| !pids.contains(&a.id.pid))
        {
            break;
        }
        assert!(
            Instant::now() < end,
            "daemon retained exited owned identities"
        );
        thread::sleep(Duration::from_millis(10));
    }
    daemon.graceful();
    assert!(
        !runtime.0.join("agentd.sock").exists(),
        "daemon left private socket after SIGTERM"
    );
}
#[test]
fn captured_uid_and_malformed_stat_do_not_become_healthy_agents() {
    let mut capture = Capture::new();
    let scanner = capture.scanner();
    let store = StateStore::with_instance_id("0123456789abcdef0123456789abcdef".into());
    let before = store.commit_scan(scanner.scan(None, &SystemClock));
    let pids: Vec<_> = before.agents.iter().map(|a| a.id.pid).collect();
    assert!(pids.len() >= 2);
    let wrong = unsafe { libc::geteuid() }.checked_add(1).unwrap();
    fs::write(
        capture.root.0.join(pids[0].to_string()).join("status"),
        format!("Uid:\t{wrong}\t{wrong}\t{wrong}\t{wrong}\n"),
    )
    .unwrap();
    fs::write(
        capture.root.0.join(pids[1].to_string()).join("stat"),
        "malformed\n",
    )
    .unwrap();
    let next = scanner.scan(Some(&before), &SystemClock);
    assert_eq!(next.scan.state, ScanState::Degraded);
    assert!(
        !next.agents.iter().any(|a| a.id.pid == pids[0]),
        "foreign UID accepted"
    );
    let uncertain = next.agents.iter().find(|a| a.id.pid == pids[1]).unwrap();
    assert_eq!(
        uncertain.presence.state,
        agentd::model::PresenceState::Unknown
    );
    assert_eq!(next.agents.len(), before.agents.len() - 1);
    for child in &mut capture.children {
        reap(&child.stop());
    }
}
#[test]
fn captured_reused_pid_does_not_inherit_activity_or_accept_old_identity() {
    use agentd::model::ActivityState;
    let mut capture = Capture::new();
    let scanner = capture.scanner();
    let store = StateStore::with_instance_id("0123456789abcdef0123456789abcdef".into());
    let before = store.commit_scan(scanner.scan(None, &SystemClock));
    let old = before.agents[0].id;
    store
        .apply_activity(old, ActivityState::NeedsAttention, &SystemClock)
        .unwrap();
    let path = capture.root.0.join(old.pid.to_string()).join("stat");
    let original = fs::read_to_string(&path).unwrap();
    let split = original.rfind(')').unwrap() + 1;
    let mut fields: Vec<String> = original[split..]
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    fields[19] = old.start_time_ticks.checked_add(1).unwrap().to_string();
    fs::write(
        &path,
        format!("{} {}\n", &original[..split], fields.join(" ")),
    )
    .unwrap();
    let changed =
        store.commit_scan(scanner.scan(store.current_snapshot().as_deref(), &SystemClock));
    let new = changed.agents.iter().find(|a| a.id.pid == old.pid).unwrap();
    assert_ne!(new.id, old);
    assert_eq!(
        new.activity.state,
        ActivityState::Unknown,
        "reused PID inherited stale activity"
    );
    assert!(
        store
            .apply_activity(old, ActivityState::Active, &SystemClock)
            .is_err(),
        "stale identity accepted"
    );
    for child in &mut capture.children {
        reap(&child.stop());
    }
}
