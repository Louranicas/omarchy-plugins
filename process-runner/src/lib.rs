//! Bounded one-shot child execution with exact argv and private process-group custody.
//! Not an OS sandbox or a streaming provider transport. No shell is inserted.
mod reaper;
pub use reaper::{CleanupReceipt, CleanupState, MAX_CHILDREN};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A slot reserved before spawning a separately managed long-lived child.
/// Handoff never sends signals; the lifecycle owner must perform group cleanup
/// while it still has WNOWAIT custody. Only the reaper can report actual reap.
pub struct ReapReservation(reaper::Permit);
impl ReapReservation {
    pub fn reserve() -> Result<Self, Error> {
        reaper::reserve().map(Self)
    }
    pub fn handoff<T: Send + 'static>(self, child: Child, retained: T) -> CleanupReceipt {
        let receipt = CleanupReceipt::new();
        reaper::transfer(child, receipt.clone(), self.0, Box::new(retained));
        receipt
    }
    /// Lost custody is quarantined, never polled or signalled by numeric PID.
    pub fn quarantine<T: Send + 'static>(self, child: Child, retained: T) -> CleanupReceipt {
        let receipt = CleanupReceipt::new();
        receipt.set(CleanupState::CustodyLost);
        reaper::transfer(child, receipt.clone(), self.0, Box::new(retained));
        receipt
    }
}

pub const MAX_OUTPUT: usize = 8 * 1024 * 1024;
pub const MAX_ARGUMENT_BYTES: usize = 256 * 1024;

pub struct Request {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
    /// Environment is cleared first. Callers explicitly select variables needed
    /// by a tool; this library never logs values or inherits provider credentials.
    pub environment: Vec<(OsString, OsString)>,
    pub timeout: Duration,
    /// Combined stdout and stderr budget, checked while draining both streams.
    pub output_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRequest,
    Spawn,
    Io,
    Timeout,
    Cancelled,
    OutputLimit,
    Busy,
    CleanupPending,
    CustodyLost,
}

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A failed operation and independent cleanup status. Outstanding means resource
/// guards remain retained; it is not permission to claim child exit.
#[derive(Debug)]
pub struct Failure {
    pub reason: Error,
    pub cleanup: CleanupReceipt,
}
struct Custody {
    child: Option<Child>,
    reaped: bool,
    receipt: CleanupReceipt,
    permit: Option<reaper::Permit>,
    retained: Option<reaper::Retained>,
    #[cfg(test)]
    skip_signal: bool,
}
impl Custody {
    fn transfer(&mut self) {
        reaper::transfer(
            self.child.take().expect("owned child"),
            self.receipt.clone(),
            self.permit.take().expect("reserved before spawn"),
            self.retained.take().expect("retained guard"),
        );
    }
    fn child(&self) -> &Child {
        self.child.as_ref().expect("owned child")
    }
    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("owned child")
    }
    fn kill_group(&self) {
        #[cfg(test)]
        if self.skip_signal {
            return;
        }
        if !self.reaped {
            unsafe { libc::kill(-(self.child().id() as libc::pid_t), libc::SIGKILL) };
        }
    }
    fn exited(&mut self) -> Result<bool, Error> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child().id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            Ok(unsafe { info.si_pid() } != 0)
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
                self.receipt.set(CleanupState::CustodyLost);
            }
            Err(Error::Io)
        }
    }
    fn finish(&mut self) -> Result<ExitStatus, Error> {
        // This is called only after WNOWAIT exit observation. No blocking wait.
        if !self.exited()? {
            return Err(Error::CleanupPending);
        }
        self.kill_group();
        match self.child_mut().try_wait() {
            Ok(Some(status)) => {
                self.reaped = true;
                self.receipt.set(CleanupState::Reaped);
                Ok(status)
            }
            Ok(None) => Err(Error::CleanupPending),
            Err(error) => {
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.reaped = true;
                    self.receipt.set(CleanupState::CustodyLost);
                }
                Err(Error::Io)
            }
        }
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        if self.reaped {
            if self.receipt.state() == CleanupState::CustodyLost {
                self.transfer();
            }
            return;
        }
        // A short cooperative cleanup grace, never Child::wait. Pending custody
        // and caller resource quota move together to the pre-reserved owner.
        let deadline = Instant::now() + Duration::from_millis(20);
        loop {
            match self.exited() {
                Ok(true) => {
                    let _ = self.finish();
                }
                Ok(false) => self.kill_group(),
                Err(_) => {}
            }
            if self.reaped {
                if self.receipt.state() == CleanupState::CustodyLost {
                    self.transfer();
                }
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.transfer();
    }
}

fn nonblocking(fd: i32) -> Result<(), Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(Error::Io)
    } else {
        Ok(())
    }
}

fn validate(request: &Request) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = 0usize;
    let mut count = 0;
    for item in std::iter::once(request.program.as_os_str())
        .chain(std::iter::once(request.directory.as_os_str()))
        .chain(request.arguments.iter().map(OsString::as_os_str))
        .chain(
            request
                .environment
                .iter()
                .flat_map(|(k, v)| [k.as_os_str(), v.as_os_str()]),
        )
    {
        let data = item.as_bytes();
        bytes = bytes.checked_add(data.len()).ok_or(Error::InvalidRequest)?;
        count += 1;
        if data.contains(&0) || bytes > MAX_ARGUMENT_BYTES || count > 4096 {
            return Err(Error::InvalidRequest);
        }
    }
    if !request.program.is_absolute()
        || !request.directory.is_absolute()
        || request.timeout.is_zero()
        || request.timeout > Duration::from_secs(60)
        || request.output_limit == 0
        || request.output_limit > MAX_OUTPUT
        || request
            .environment
            .iter()
            .any(|(k, _)| k.is_empty() || k.as_bytes().contains(&b'='))
    {
        return Err(Error::InvalidRequest);
    }
    let mut keys = std::collections::BTreeSet::new();
    if request.environment.iter().any(|(k, _)| !keys.insert(k)) {
        return Err(Error::InvalidRequest);
    }
    Ok(())
}

fn drain(
    reader: &mut impl Read,
    target: &mut Vec<u8>,
    total: &mut usize,
    limit: usize,
) -> Result<bool, Error> {
    // Bound each pass so an always-ready stdout cannot starve stderr/cancellation.
    let mut buffer = [0u8; 8192];
    for _ in 0..8 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                *total += n;
                if *total > limit {
                    return Err(Error::OutputLimit);
                }
                target.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Io),
        }
    }
    Ok(false)
}

/// Executes one child and drains both output pipes. The deadline/cancellation is
/// checked between bounded nonblocking drain passes. Spawn may block beyond it;
/// cleanup adds a 20ms cooperative grace then retains custody in a finite reaper.
/// Errors discard captured bytes rather than logging them.
///
/// The caller must retain exclusive child reaping: no SIGCHLD=SIG_IGN,
/// SA_NOCLDWAIT or competing waitpid(-1). Descendants that deliberately escape
/// their process group are outside this custody model. This is not a sandbox.
pub fn run(request: &Request, cancelled: &AtomicBool) -> Result<Output, Error> {
    run_retaining(request, cancelled, ()).map_err(|failure| match failure.cleanup.state() {
        CleanupState::Outstanding => Error::CleanupPending,
        CleanupState::CustodyLost => Error::CustodyLost,
        _ => failure.reason,
    })
}
/// Own resource admission through direct-child reap, even when this call returns.
/// Retained guard destructors must be bounded and panic-free. This does not prove
/// all escaped/blocked descendants are gone and does not survive process exit.
pub fn run_retaining<T: Send + 'static>(
    request: &Request,
    cancelled: &AtomicBool,
    retained: T,
) -> Result<Output, Failure> {
    let receipt = CleanupReceipt::new();
    run_inner(request, cancelled, Box::new(retained), receipt.clone()).map_err(|reason| Failure {
        reason,
        cleanup: receipt,
    })
}
fn run_inner(
    request: &Request,
    cancelled: &AtomicBool,
    retained: reaper::Retained,
    receipt: CleanupReceipt,
) -> Result<Output, Error> {
    validate(request)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let permit = reaper::reserve()?;
    let deadline = Instant::now() + request.timeout;
    let mut command = Command::new(&request.program);
    command
        .args(&request.arguments)
        .current_dir(&request.directory)
        .env_clear()
        .envs(request.environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: only one async-signal-safe syscall runs between fork and exec.
    // CLOEXEC keeps Rust's exec-error pipe usable until exec while closing all
    // accidental caller descriptors in the executed program. Never fall back.
    unsafe {
        command.pre_exec(|| {
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command.spawn().map_err(|_| Error::Spawn)?;
    receipt.set(CleanupState::Running);
    let mut custody = Custody {
        child: Some(child),
        reaped: false,
        receipt,
        permit: Some(permit),
        retained: Some(retained),
        #[cfg(test)]
        skip_signal: false,
    };
    let mut stdout = custody.child_mut().stdout.take().ok_or(Error::Io)?;
    let mut stderr = custody.child_mut().stderr.take().ok_or(Error::Io)?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let (mut out, mut err, mut total) = (Vec::new(), Vec::new(), 0);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        let out_eof = drain(&mut stdout, &mut out, &mut total, request.output_limit)?;
        let err_eof = drain(&mut stderr, &mut err, &mut total, request.output_limit)?;
        if custody.exited()? {
            // Descendants cannot keep our pipes alive after the direct child exits.
            custody.kill_group();
            if out_eof && err_eof {
                return Ok(Output {
                    status: custody.finish()?,
                    stdout: out,
                    stderr: err,
                });
            }
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell(script: &str) -> Request {
        Request {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), script.into()],
            directory: "/".into(),
            environment: vec![],
            timeout: Duration::from_secs(2),
            output_limit: 1024,
        }
    }
    fn execute(r: &Request) -> Result<Output, Error> {
        run(r, &AtomicBool::new(false))
    }
    #[test]
    fn lost_wait_custody_disarms_group_signalling() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        child.wait().unwrap();
        let mut custody = Custody {
            child: Some(child),
            reaped: false,
            receipt: CleanupReceipt::new(),
            permit: Some(reaper::reserve().unwrap()),
            retained: Some(Box::new(())),
            skip_signal: false,
        };
        assert_eq!(custody.exited(), Err(Error::Io));
        assert!(
            custody.reaped,
            "ECHILD must disarm later numeric group cleanup"
        );
    }

    #[test]
    fn interrupted_reads_have_a_bounded_pass_and_preserve_bytes() {
        struct Interrupted {
            calls: usize,
        }
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                Err(io::Error::from(io::ErrorKind::Interrupted))
            }
        }
        let mut reader = Interrupted { calls: 0 };
        let mut target = vec![42];
        let mut total = 1;
        assert!(!drain(&mut reader, &mut target, &mut total, 100).unwrap());
        assert_eq!(reader.calls, 8);
        assert_eq!(target, [42]);
        assert_eq!(total, 1);
    }

    #[test]
    fn exact_combined_limit_and_non_utf8_bytes_are_preserved() {
        let mut r = shell("printf '\\377\\376'; printf ab >&2");
        r.output_limit = 4;
        let output = execute(&r).unwrap();
        assert_eq!(output.stdout, [255, 254]);
        assert_eq!(output.stderr, b"ab");
    }

    #[test]
    fn cancellation_is_observed_while_both_streams_are_active() {
        let mut r =
            shell("while :; do printf stdout; printf stderr >&2; /usr/bin/sleep 0.001; done");
        r.output_limit = MAX_OUTPUT;
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                cancelled.store(true, Ordering::Release);
            });
            let start = Instant::now();
            assert_eq!(run(&r, &cancelled).unwrap_err(), Error::Cancelled);
            assert!(start.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn detached_stdin_descendant_cannot_keep_parent_result_open() {
        let start = Instant::now();
        let output = execute(&shell("(/usr/bin/sleep 30) < /dev/null & exit 23")).unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn spawn_failures_and_argument_budgets_are_explicit() {
        let mut r = shell("exit 0");
        r.program = "/definitely-absent-omarchy-test-program".into();
        assert_eq!(execute(&r).unwrap_err(), Error::Spawn);
        r.program = "/bin/sh".into();
        r.arguments = vec!["x".repeat(MAX_ARGUMENT_BYTES).into()];
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
        r.arguments = vec!["".into(); 4097];
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
        r.arguments.clear();
        r.timeout = Duration::ZERO;
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
        r.timeout = Duration::from_secs(61);
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
    }

    #[test]
    fn exact_arguments_are_not_shell_interpolated() {
        let mut r = shell("");
        r.program = "/usr/bin/printf".into();
        r.arguments = vec!["%s".into(), "$(touch /not-created); $HOME `id`\n".into()];
        let out = execute(&r).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"$(touch /not-created); $HOME `id`\n");
    }
    #[test]
    fn exit_code_and_both_streams_preserved() {
        let out = execute(&shell("printf out; printf err >&2; exit 23")).unwrap();
        assert_eq!(out.status.code(), Some(23));
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
    }
    #[test]
    fn aggregate_output_limit_and_noisy_child() {
        let mut r = shell("printf 1234; printf 5678 >&2");
        r.output_limit = 7;
        assert_eq!(execute(&r).unwrap_err(), Error::OutputLimit);
        r.arguments[1] = "while :; do printf 123456789; done".into();
        assert_eq!(execute(&r).unwrap_err(), Error::OutputLimit);
    }
    #[test]
    fn deadline_cleans_up_child_with_open_pipes() {
        let mut r = shell("/usr/bin/sleep 30");
        r.timeout = Duration::from_millis(30);
        let start = Instant::now();
        assert_eq!(execute(&r).unwrap_err(), Error::Timeout);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn exited_parent_cannot_leave_descendant_holding_pipe() {
        let start = Instant::now();
        let out = execute(&shell("/usr/bin/sleep 30 & printf done; exit 0")).unwrap();
        assert_eq!(out.stdout, b"done");
        assert!(out.status.success());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn cancellation_before_and_during_child() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            run(&shell("exit 0"), &cancelled).unwrap_err(),
            Error::Cancelled
        );
        cancelled.store(false, Ordering::Release);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(25));
                cancelled.store(true, Ordering::Release);
            });
            assert_eq!(
                run(&shell("/usr/bin/sleep 30"), &cancelled).unwrap_err(),
                Error::Cancelled
            );
        });
    }
    #[test]
    fn invalid_requests_fail_before_spawn() {
        let mut r = shell("exit 0");
        r.program = "sh".into();
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
        r.program = "/bin/sh".into();
        r.arguments.push("bad\0value".into());
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
        r.arguments.pop();
        r.environment = vec![("X".into(), "a".into()), ("X".into(), "b".into())];
        assert_eq!(execute(&r).unwrap_err(), Error::InvalidRequest);
    }
    #[test]
    fn environment_is_explicit_and_stdin_is_closed() {
        let mut r = shell(
            "printf '%s/%s' \"$ONLY_THIS\" \"${UNSET_TEST-default}\"; read value; test $? -ne 0",
        );
        r.environment.push(("ONLY_THIS".into(), "chosen".into()));
        let out = execute(&r).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"chosen/default");
    }
    #[test]
    fn inherited_non_cloexec_host_descriptor_does_not_reach_child() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let file = std::fs::File::open("/dev/null").unwrap();
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 100) };
        assert!(raw >= 100);
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let output = execute(&shell(&format!(
            "test ! -e /proc/self/fd/{}",
            fd.as_raw_fd()
        )))
        .unwrap();
        assert!(output.status.success());
        assert!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0,
            "parent descriptor remains live"
        );
    }
    #[test]
    fn failed_inline_cleanup_transfers_child_and_retains_admission() {
        use std::sync::{Arc, atomic::AtomicUsize};
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let child = Command::new("/usr/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let receipt = CleanupReceipt::new();
        receipt.set(CleanupState::Running);
        let released = Arc::new(AtomicUsize::new(0));
        let custody = Custody {
            child: Some(child),
            reaped: false,
            receipt: receipt.clone(),
            permit: Some(reaper::reserve().unwrap()),
            retained: Some(Box::new(Guard(released.clone()))),
            skip_signal: true,
        };
        let start = Instant::now();
        drop(custody);
        let elapsed = start.elapsed();
        let before_state = receipt.state();
        let before_released = released.load(Ordering::Acquire);
        // Controlled fault injection withheld the normal kill. The child remains
        // owned by the reaper, so its numeric PID is still pinned here.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while released.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(before_state, CleanupState::Outstanding);
        assert_eq!(before_released, 0);
        assert_eq!(receipt.state(), CleanupState::Reaped);
        assert_eq!(released.load(Ordering::Acquire), 1);
    }
    #[test]
    fn retained_guard_is_released_on_pre_spawn_rejection() {
        use std::sync::{Arc, atomic::AtomicUsize};
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let released = Arc::new(AtomicUsize::new(0));
        let mut request = shell("exit 0");
        request.program = "relative".into();
        let failure =
            run_retaining(&request, &AtomicBool::new(false), Guard(released.clone())).unwrap_err();
        assert_eq!(failure.reason, Error::InvalidRequest);
        assert_eq!(failure.cleanup.state(), CleanupState::NotSpawned);
        assert_eq!(released.load(Ordering::Acquire), 1);
    }
}
