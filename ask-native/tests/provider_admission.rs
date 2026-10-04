use ask_native::backend::Handle;
use ask_runtime::provider::ProviderProfile;
use modal_runtime::{process_identity::Pinned, readiness::ParentWatch};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
struct OwnedParent {
    child: Option<std::process::Child>,
    reservation: Option<omarchy_process_runner::ReapReservation>,
}
impl OwnedParent {
    fn new() -> Self {
        let reservation = omarchy_process_runner::ReapReservation::reserve().unwrap();
        let child = std::process::Command::new("/usr/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            reservation: Some(reservation),
        }
    }
    fn id(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    fn stop(&mut self) -> Option<omarchy_process_runner::CleanupReceipt> {
        self.child.take().map(|mut child| {
            let _ = child.kill(); // Exclusive unreaped direct-child custody; no numeric group signals.
            self.reservation.take().unwrap().handoff(child, ())
        })
    }
}
impl Drop for OwnedParent {
    fn drop(&mut self) {
        self.stop();
    }
}
fn parent(pid: u32) -> ParentWatch {
    ParentWatch::pin(pid, Pinned::capture(pid).unwrap().identity().start_ticks).unwrap()
}
fn pending(root: &Path, watcher: ParentWatch) -> Handle {
    let script = format!(
        "import os;open({:?},'w').write('executed');os.execv({:?},[{:?},'--fixture-provider'])",
        root.join("executed").to_str().unwrap(),
        env!("CARGO_BIN_EXE_ask-native"),
        env!("CARGO_BIN_EXE_ask-native")
    );
    let profile=ProviderProfile::parse(&serde_json::to_vec(&serde_json::json!({"version":1,"harness":"codex","adapter_argv":["/usr/bin/python3","-c",script],"harness_executable":env!("CARGO_BIN_EXE_ask-native"),"cwd":root})).unwrap()).unwrap();
    Handle::start_pending_guarded(
        profile.launch().clone(),
        profile.environment(|_| None).unwrap(),
        watcher,
        profile.preflight().unwrap(),
    )
}
#[test]
fn pending_provider_launches_only_after_explicit_admission() {
    let t = tempfile::tempdir().unwrap();
    let h = pending(t.path(), parent(std::process::id()));
    std::thread::sleep(Duration::from_millis(80));
    let executed_before_admission = t.path().join("executed").exists();
    assert!(h.admit_after_managed_grant());
    let end = Instant::now() + Duration::from_secs(3);
    let mut ready = false;
    while Instant::now() < end {
        if h.views.take().is_some_and(|v| v.ready) {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let clean = h.finish();
    assert!(ready);
    assert!(clean);
    assert!(!executed_before_admission);
    assert!(t.path().join("executed").exists());
}
#[test]
fn stopped_pending_provider_never_launches_after_late_admission() {
    let t = tempfile::tempdir().unwrap();
    let h = pending(t.path(), parent(std::process::id()));
    h.stop();
    assert!(!h.admit_after_managed_grant());
    assert!(h.finish());
    assert!(!t.path().join("executed").exists());
}
#[test]
fn dead_parent_before_admission_never_launches_provider() {
    let t = tempfile::tempdir().unwrap();
    let mut child = OwnedParent::new();
    let h = pending(t.path(), parent(child.id()));
    let receipt = child.stop().unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    while receipt.state() != omarchy_process_runner::CleanupState::Reaped && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        receipt.state(),
        omarchy_process_runner::CleanupState::Reaped
    );
    h.admit_after_managed_grant();
    std::thread::sleep(Duration::from_millis(80));
    let executed = t.path().join("executed").exists();
    assert!(h.finish());
    assert!(!executed);
    assert!(!t.path().join("executed").exists());
}
#[test]
fn delayed_admission_rechecks_changed_project_before_spawn() {
    let t = tempfile::tempdir().unwrap();
    let project = t.path().join("project");
    fs::create_dir(&project).unwrap();
    let h = pending(&project, parent(std::process::id()));
    fs::rename(&project, t.path().join("old")).unwrap();
    fs::create_dir(&project).unwrap();
    assert!(h.admit_after_managed_grant());
    let end = Instant::now() + Duration::from_secs(3);
    let mut rejected = false;
    while Instant::now() < end {
        if h.views
            .take()
            .is_some_and(|v| v.status == "Provider paths changed before launch")
        {
            rejected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let clean = h.finish();
    assert!(rejected);
    assert!(!clean);
    assert!(!project.join("executed").exists());
    assert!(!t.path().join("old/executed").exists());
}
