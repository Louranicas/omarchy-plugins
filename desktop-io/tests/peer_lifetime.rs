//! Child fixtures have bounded readiness and retained cleanup on assertion unwind.
use desktop_io::{Endpoint, Error, Query};
use omarchy_process_runner::{CleanupReceipt, CleanupState, ReapReservation};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
const BUDGET: Duration = Duration::from_secs(2);
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Runtime(PathBuf);
impl Runtime {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "desktop-peer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(path.join("hypr/test_1")).unwrap();
        Self(path)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct FixtureChild {
    child: Option<Child>,
    reservation: Option<ReapReservation>,
    receipt: Option<CleanupReceipt>,
}
impl FixtureChild {
    fn spawn(root: &Path, mode: &str) -> Self {
        let reservation = ReapReservation::reserve().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_child", "--ignored"])
            .env("DESKTOP_PEER_TEST_ROOT", root)
            .env("DESKTOP_PEER_TEST_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            reservation: Some(reservation),
            receipt: None,
        }
    }
    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    fn stop(&mut self) -> CleanupReceipt {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            self.receipt = Some(self.reservation.take().unwrap().handoff(child, ()));
        }
        self.receipt.as_ref().unwrap().clone()
    }
}
impl Drop for FixtureChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}
fn reaped(receipt: &CleanupReceipt) {
    let deadline = Instant::now() + BUDGET;
    while receipt.state() != CleanupState::Reaped {
        assert!(
            Instant::now() < deadline,
            "child not reaped: {:?}",
            receipt.state()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn accept(listener: &UnixListener) -> io::Result<UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + BUDGET;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(300)))?;
                stream.set_write_timeout(Some(Duration::from_millis(300)))?;
                return Ok(stream);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1))
            }
            Err(e) => return Err(e),
        }
    }
}
#[test]
#[ignore = "subprocess-only fixture; invoked by parent peer lifecycle tests"]
fn peer_child() {
    let root = PathBuf::from(std::env::var_os("DESKTOP_PEER_TEST_ROOT").unwrap());
    let listener = UnixListener::bind(root.join("hypr/test_1/.socket.sock")).unwrap();
    // Keep a genuine socket owned by this PID across exec. If executable checks
    // regress, the query times out rather than accidentally failing on no socket.
    let fd = listener.as_raw_fd();
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
    let mut control = UnixStream::connect(root.join("control.sock")).unwrap();
    if std::env::var("DESKTOP_PEER_TEST_MODE").unwrap() == "stall" {
        thread::sleep(Duration::from_secs(30));
        return;
    }
    control.write_all(b"R").unwrap();
    let mut command = [0];
    control.set_read_timeout(Some(BUDGET)).unwrap();
    control.read_exact(&mut command).unwrap();
    assert_eq!(&command, b"E");
    let error = Command::new("/bin/sleep").arg("5").exec();
    panic!("exec failed: {error}");
}
#[test]
fn same_pid_exec_refuses_before_transmitting_to_retained_socket() {
    let runtime = Runtime::new();
    let listener = UnixListener::bind(runtime.0.join("control.sock")).unwrap();
    let mut child = FixtureChild::spawn(&runtime.0, "exec");
    let mut control = accept(&listener).unwrap();
    let mut ready = [0];
    control.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"R");
    let endpoint = Endpoint::discover(&runtime.0, "test_1", child.pid()).unwrap();
    control.write_all(b"E").unwrap();
    let expected = fs::metadata("/bin/sleep").unwrap();
    let deadline = Instant::now() + BUDGET;
    loop {
        let current = fs::metadata(format!("/proc/{}/exe", child.pid())).unwrap();
        if (current.dev(), current.ino()) == (expected.dev(), expected.ino()) {
            break;
        }
        assert!(Instant::now() < deadline, "child failed to exec");
        thread::sleep(Duration::from_millis(1));
    }
    let result = endpoint.query(Query::Clients, Duration::from_millis(30));
    reaped(&child.stop());
    assert!(matches!(result, Err(Error::Unauthenticated)), "{result:?}");
}
#[test]
fn stalled_readiness_has_deadline_and_child_is_reaped_on_unwind() {
    let runtime = Runtime::new();
    let listener = UnixListener::bind(runtime.0.join("control.sock")).unwrap();
    let mut child = FixtureChild::spawn(&runtime.0, "stall");
    let mut control = accept(&listener).unwrap();
    let started = Instant::now();
    let error = control.read_exact(&mut [0]).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < BUDGET);
    let receipt = child.stop();
    // Exercise the same guard on unwind after cleanup was already requested.
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = child;
            panic!("fixture assertion");
        }))
        .is_err()
    );
    reaped(&receipt);
}
#[test]
fn assertion_unwind_reaps_child_without_explicit_stop() {
    let runtime = Runtime::new();
    let listener = UnixListener::bind(runtime.0.join("control.sock")).unwrap();
    let child = FixtureChild::spawn(&runtime.0, "stall");
    let pid = child.pid();
    let _control = accept(&listener).unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = child;
            panic!("fixture assertion");
        }))
        .is_err()
    );
    let deadline = Instant::now() + BUDGET;
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "child survived unwind");
        thread::sleep(Duration::from_millis(1));
    }
}
