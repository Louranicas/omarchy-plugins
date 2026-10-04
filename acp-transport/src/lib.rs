//! ACP v1 newline JSON-RPC stdio transport. This layer grants no tool permissions.
//! Serialized worker API; every operation has a deadline and cancellation flag.
mod reaper;
mod wire;
pub use reaper::{MAX_CHILDREN, ReapState, Receipt as ReapReceipt};
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
pub use wire::{decode, encode};
pub const MAX_FRAME: usize = 1024 * 1024;
const MAX_QUEUE: usize = 64;
const MAX_QUEUED_BYTES: usize = 4 * MAX_FRAME;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Spawn,
    Io,
    Protocol,
    Limit,
    Deadline,
    Cancelled,
    Exited,
    Closed,
}
pub struct Launch {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
    /// Explicit environment; authentication inherited only by caller decision.
    pub environment: Vec<(OsString, OsString)>,
}
pub struct Transport {
    child: Option<Child>,
    permit: Option<reaper::Permit>,
    reap: ReapReceipt,
    input: ChildStdin,
    output: ChildStdout,
    diagnostics: ChildStderr,
    partial: Vec<u8>,
    stdout_eof: bool,
    partial_since: Option<Instant>,
    messages: VecDeque<(usize, Value)>,
    queued_bytes: usize,
    closed: bool,
}
fn nonblocking(fd: i32) -> Result<(), Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(Error::Io)
    } else {
        Ok(())
    }
}
impl Transport {
    pub fn spawn(launch: &Launch) -> Result<Self, Error> {
        if !launch.executable.is_absolute()
            || !launch.directory.is_absolute()
            || launch.arguments.len() > 256
            || launch.environment.len() > 256
        {
            return Err(Error::Invalid);
        }
        let mut bytes = 0usize;
        let mut keys = std::collections::BTreeSet::new();
        for s in std::iter::once(launch.executable.as_os_str())
            .chain(std::iter::once(launch.directory.as_os_str()))
            .chain(launch.arguments.iter().map(OsString::as_os_str))
            .chain(
                launch
                    .environment
                    .iter()
                    .flat_map(|(k, v)| [k.as_os_str(), v.as_os_str()]),
            )
        {
            bytes = bytes.checked_add(s.len()).ok_or(Error::Invalid)?;
            if s.as_bytes().contains(&0) || bytes > 256 * 1024 {
                return Err(Error::Invalid);
            }
        }
        if launch
            .environment
            .iter()
            .any(|(k, _)| k.is_empty() || k.as_bytes().contains(&b'=') || !keys.insert(k))
        {
            return Err(Error::Invalid);
        }
        let permit = reaper::reserve()?;
        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.arguments)
            .current_dir(&launch.directory)
            .env_clear()
            .envs(launch.environment.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // Only stdio crosses this boundary. CLOEXEC marks the child descriptor
        // table without changing parent FDs or closing Rust's exec-error pipe early.
        // The callback makes only an async-signal-safe Linux syscall.
        unsafe {
            command.pre_exec(|| {
                if libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as i32) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        let mut child = command.spawn().map_err(|_| Error::Spawn)?;
        // Piped descriptors are guaranteed by the command configuration.
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let diagnostics = child.stderr.take().expect("piped stderr");
        let transport = Self {
            child: Some(child),
            permit: Some(permit),
            reap: ReapReceipt::new(),
            input,
            output,
            diagnostics,
            partial: vec![],
            stdout_eof: false,
            partial_since: None,
            messages: VecDeque::new(),
            queued_bytes: 0,
            closed: false,
        };
        nonblocking(transport.input.as_raw_fd())?;
        nonblocking(transport.output.as_raw_fd())?;
        nonblocking(transport.diagnostics.as_raw_fd())?;
        Ok(transport)
    }
    fn deadline(budget: Duration) -> Result<Instant, Error> {
        if budget.is_zero() || budget > Duration::from_secs(60) {
            Err(Error::Invalid)
        } else {
            Instant::now().checked_add(budget).ok_or(Error::Invalid)
        }
    }
    fn check(&self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        Ok(())
    }
    fn discard_stderr(&mut self, discarded: &mut usize) -> Result<(), Error> {
        let mut buf = [0u8; 8192];
        for _ in 0..8 {
            match self.diagnostics.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    *discarded += n;
                    if *discarded > MAX_FRAME {
                        return Err(Error::Limit);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::Io),
            }
        }
        Ok(())
    }
    fn ingest(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.partial.is_empty() && !bytes.is_empty() {
            self.partial_since = Some(Instant::now());
        }
        self.partial.extend_from_slice(bytes);
        while let Some(at) = self.partial.iter().position(|b| *b == b'\n') {
            if at > MAX_FRAME
                || self.messages.len() >= MAX_QUEUE
                || self.queued_bytes + at > MAX_QUEUED_BYTES
            {
                return Err(Error::Limit);
            }
            let value = decode(&self.partial[..at])?;
            self.messages.push_back((at, value));
            self.queued_bytes += at;
            self.partial.drain(..=at);
            self.partial_since = if self.partial.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
        if self.partial.len() > MAX_FRAME {
            return Err(Error::Limit);
        }
        Ok(())
    }
    fn read_frames(&mut self) -> Result<(), Error> {
        if self
            .partial_since
            .is_some_and(|start| start.elapsed() >= Duration::from_secs(1))
        {
            return Err(Error::Deadline);
        }
        if self.stdout_eof {
            return if self.messages.is_empty() {
                Err(Error::Exited)
            } else {
                Ok(())
            };
        }
        let mut buf = [0u8; 8192];
        for _ in 0..8 {
            match self.output.read(&mut buf) {
                Ok(0) => {
                    self.stdout_eof = true;
                    // Deliver complete frames already received before surfacing EOF, even
                    // if a final incomplete frame follows them. Never pretend partial is JSON.
                    return if self.messages.is_empty() {
                        Err(Error::Exited)
                    } else {
                        Ok(())
                    };
                }
                Ok(n) => self.ingest(&buf[..n])?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::Io),
            }
        }
        Ok(())
    }
    fn wait(&self, write: bool, deadline: Instant) -> Result<(), Error> {
        let mut fds = [
            libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.diagnostics.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if write { self.input.as_raw_fd() } else { -1 },
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(10) as i32;
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            Err(Error::Io)
        } else {
            Ok(())
        }
    }
    /// Any failed I/O operation poisons and tears down the session. A timeout
    /// cannot leave a partially written request available for accidental replay.
    pub fn send(
        &mut self,
        value: &Value,
        budget: Duration,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let bytes = encode(value)?;
        let deadline = Self::deadline(budget)?;
        let result = (|| {
            let (mut at, mut discarded) = (0, 0);
            while at < bytes.len() {
                self.check(deadline, cancel)?;
                self.discard_stderr(&mut discarded)?;
                self.read_frames()?;
                match self.input.write(&bytes[at..]) {
                    Ok(0) => return Err(Error::Exited),
                    Ok(n) => at += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(Error::Io),
                }
                if at < bytes.len() {
                    self.wait(true, deadline)?;
                }
            }
            self.check(deadline, cancel)
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Nonblocking worker pump. Idle is not failure; an incomplete frame has a
    /// fixed one-second deadline across calls, so byte drips cannot renew it.
    pub fn poll_receive(&mut self, cancel: &AtomicBool) -> Result<Option<Value>, Error> {
        let result = (|| {
            self.check(Instant::now() + Duration::from_secs(1), cancel)?;
            if let Some((size, value)) = self.messages.pop_front() {
                self.queued_bytes -= size;
                return Ok(Some(value));
            }
            self.discard_stderr(&mut 0)?;
            self.read_frames()?;
            Ok(self.messages.pop_front().map(|(size, value)| {
                self.queued_bytes -= size;
                value
            }))
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn receive(&mut self, budget: Duration, cancel: &AtomicBool) -> Result<Value, Error> {
        let deadline = Self::deadline(budget)?;
        let result = (|| {
            let mut discarded = 0;
            loop {
                self.check(deadline, cancel)?;
                if let Some((size, value)) = self.messages.pop_front() {
                    self.queued_bytes -= size;
                    return Ok(value);
                }
                self.discard_stderr(&mut discarded)?;
                self.read_frames()?;
                if self.messages.is_empty() {
                    self.wait(false, deadline)?;
                }
            }
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Cooperative TERM while retaining direct-child custody for later KILL/reap.
    pub fn terminate(&mut self) -> Result<(), Error> {
        let child = self.child.as_ref().ok_or(Error::Closed)?;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        for _ in 0..8 {
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                let signalled = unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
                return if signalled == 0
                    || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    Ok(())
                } else {
                    Err(Error::Io)
                };
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(Error::Io);
            }
        }
        Err(Error::Io)
    }
    pub fn reap_receipt(&self) -> ReapReceipt {
        self.reap.clone()
    }
    /// Signal once while direct-child custody is retained; transfer to the bounded
    /// nonblocking reaper. Outstanding is explicit, never claimed as successful reap.
    pub fn close(&mut self) -> ReapState {
        self.closed = true;
        self.messages.clear();
        self.partial.clear();
        self.queued_bytes = 0;
        let Some(child) = self.child.take() else {
            return self.reap.state();
        };
        let permit = self.permit.take().expect("child admission ownership");
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let mut custody = false;
        for _ in 0..8 {
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                custody = true;
                break;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                reaper::quarantine(child, self.reap.clone(), permit);
                return self.reap.state();
            }
            if error.kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
        // WNOWAIT pins the direct child's PID, including after natural exit. Never
        // signal a numeric process group once another owner may have reaped it.
        if custody {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        }
        reaper::transfer(child, self.reap.clone(), permit);
        self.reap.state()
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn child(script: &str) -> Transport {
        Transport::spawn(&Launch {
            executable: "/usr/bin/python3".into(),
            arguments: vec!["-u".into(), "-c".into(), script.into()],
            directory: "/".into(),
            environment: vec![],
        })
        .unwrap()
    }
    fn no() -> AtomicBool {
        AtomicBool::new(false)
    }
    #[test]
    fn round_trip_exact_unicode_message() {
        let mut t = child("import sys\nfor line in sys.stdin: print(line.strip())");
        let v = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"α 日本語\\n"}});
        t.send(&v, Duration::from_secs(1), &no()).unwrap();
        assert_eq!(t.receive(Duration::from_secs(1), &no()).unwrap(), v);
    }
    #[test]
    fn malformed_duplicate_batch_and_nonprotocol_lines_rejected() {
        for bytes in [
            b"[]".as_slice(),
            b"hello",
            br#"{"jsonrpc":"2.0","method":"x","params":{"a":1,"a":2}}"#,
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
        ] {
            assert_eq!(decode(bytes).unwrap_err(), Error::Protocol);
        }
    }
    #[test]
    fn stderr_does_not_contaminate_stdout() {
        let mut t = child(
            "import sys\nprint('private diagnostic',file=sys.stderr)\nprint('{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}')",
        );
        assert_eq!(
            t.receive(Duration::from_secs(1), &no()).unwrap()["method"],
            "ready"
        );
    }
    #[test]
    fn timeout_and_cancellation_poison_child() {
        let mut t = child("import time;time.sleep(30)");
        assert_eq!(
            t.receive(Duration::from_millis(20), &no()).unwrap_err(),
            Error::Deadline
        );
        assert_eq!(
            t.receive(Duration::from_secs(1), &no()).unwrap_err(),
            Error::Closed
        );
        let mut t = child("import time;time.sleep(30)");
        assert_eq!(
            t.receive(Duration::from_secs(1), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
    }
    #[test]
    fn oversized_frame_and_bounded_message_queue() {
        let mut t = child("import sys\nsys.stdout.write('x'*1100000);sys.stdout.flush()");
        assert_eq!(
            t.receive(Duration::from_secs(1), &no()).unwrap_err(),
            Error::Limit
        );
        let mut t = child("import time;time.sleep(30)");
        let frame = b"{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}\n";
        for _ in 0..MAX_QUEUE {
            t.ingest(frame).unwrap();
        }
        assert_eq!(t.messages.len(), MAX_QUEUE);
        assert_eq!(t.ingest(frame), Err(Error::Limit));
        assert_eq!(t.messages.len(), MAX_QUEUE);
    }
    #[test]
    fn stdout_log_and_partial_eof_fail_closed() {
        let mut t = child("print('not JSON')");
        assert_eq!(
            t.receive(Duration::from_secs(1), &no()).unwrap_err(),
            Error::Protocol
        );
        let mut t = child("print('{',end='')");
        assert_eq!(
            t.receive(Duration::from_secs(1), &no()).unwrap_err(),
            Error::Exited
        );
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    #[test]
    fn complete_messages_survive_trailing_partial_eof() {
        let mut t=Transport::spawn(&Launch { executable:"/usr/bin/python3".into(), arguments:vec!["-u".into(),"-c".into(),"import sys;sys.stdout.write('{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n{');sys.stdout.flush()".into()], directory:"/".into(),environment:vec![] }).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(t.receive(Duration::from_secs(1), &cancel).unwrap()["id"], 1);
        assert_eq!(
            t.receive(Duration::from_secs(1), &cancel),
            Err(Error::Exited)
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    fn launch(script: &str) -> Launch {
        Launch {
            executable: "/usr/bin/python3".into(),
            arguments: vec!["-u".into(), "-c".into(), script.into()],
            directory: "/".into(),
            environment: vec![],
        }
    }
    fn await_reap(receipt: &ReapReceipt) {
        let end = Instant::now() + Duration::from_secs(2);
        while receipt.state() == ReapState::Outstanding && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(receipt.state(), ReapState::Reaped);
    }
    #[test]
    fn close_returns_promptly_and_receipt_observes_actual_reap() {
        let mut t = Transport::spawn(&launch("import time;time.sleep(30)")).unwrap();
        let receipt = t.reap_receipt();
        let start = Instant::now();
        assert!(matches!(
            t.close(),
            ReapState::Outstanding | ReapState::Reaped
        ));
        assert!(start.elapsed() < Duration::from_millis(100));
        await_reap(&receipt);
        assert_eq!(t.close(), ReapState::Reaped);
    }
    #[test]
    fn dropped_transport_transfers_custody_to_reaper() {
        let t = Transport::spawn(&launch("import time;time.sleep(30)")).unwrap();
        let receipt = t.reap_receipt();
        drop(t);
        await_reap(&receipt);
    }
    #[test]
    fn one_live_unreapable_yet_entry_does_not_block_other_children() {
        // A live sleeping child models a temporarily non-reapable entry without requiring D-state.
        let a = Transport::spawn(&launch("import time;time.sleep(0.2)")).unwrap();
        let mut a = a;
        let receipt = a.reap_receipt();
        let child = a.child.take().unwrap();
        let permit = a.permit.take().unwrap();
        reaper::transfer(child, receipt.clone(), permit);
        let mut b = Transport::spawn(&launch("import time;time.sleep(30)")).unwrap();
        let rb = b.reap_receipt();
        b.close();
        await_reap(&rb);
        await_reap(&receipt);
    }
    #[test]
    fn blocked_input_has_absolute_deadline_and_poisoned_session() {
        let mut t = Transport::spawn(&launch("import time;time.sleep(30)")).unwrap();
        let v =
            serde_json::json!({"jsonrpc":"2.0","method":"x","params":{"text":"x".repeat(900_000)}});
        let start = Instant::now();
        assert_eq!(
            t.send(&v, Duration::from_millis(30), &AtomicBool::new(false)),
            Err(Error::Deadline)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            t.send(&v, Duration::from_secs(1), &AtomicBool::new(false)),
            Err(Error::Closed)
        );
        await_reap(&t.reap_receipt());
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    #[test]
    fn idle_poll_is_not_a_timeout_and_partial_age_is_absolute() {
        let mut t = Transport::spawn(&Launch {
            executable: "/usr/bin/python3".into(),
            arguments: vec!["-c".into(), "import time;time.sleep(30)".into()],
            directory: "/".into(),
            environment: vec![],
        })
        .unwrap();
        assert_eq!(t.poll_receive(&AtomicBool::new(false)).unwrap(), None);
        t.ingest(b"{").unwrap();
        t.partial_since = Some(Instant::now() - Duration::from_secs(2));
        t.ingest(b" ").unwrap();
        assert_eq!(
            t.poll_receive(&AtomicBool::new(false)),
            Err(Error::Deadline)
        );
        assert_eq!(t.poll_receive(&AtomicBool::new(false)), Err(Error::Closed));
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    use std::os::fd::FromRawFd;
    #[test]
    fn only_stdio_is_inherited_even_when_caller_leaves_host_fd_inheritable() {
        let source = std::fs::File::open("/dev/null").unwrap();
        let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 512) };
        assert!(raw >= 512);
        let leaked = unsafe { std::fs::File::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let script = format!(
            "import os,json\ntry:\n os.fstat({raw}); visible=True\nexcept OSError:\n visible=False\nprint(json.dumps({{'jsonrpc':'2.0','id':1,'result':{{'visible':visible}}}}),flush=True)"
        );
        let mut t = Transport::spawn(&Launch {
            executable: "/usr/bin/python3".into(),
            arguments: vec!["-u".into(), "-c".into(), script.into()],
            directory: "/".into(),
            environment: vec![],
        })
        .unwrap();
        assert_eq!(
            t.receive(Duration::from_secs(1), &AtomicBool::new(false))
                .unwrap()["result"]["visible"],
            false
        );
        assert_eq!(
            unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
