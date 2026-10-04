//! Dedicated child channel: a map report is necessary, never compositor proof.
use crate::socket::{Connection, directory};
use modal_contract::Fence;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
/// Dedicated presenters check this lifetime before activating and during UI ticks.
/// A successful initial map acknowledgment does not establish continuing parent life.
pub struct ParentWatch(crate::process_identity::Pinned);
impl ParentWatch {
    pub fn pin(pid: u32, start_ticks: u64) -> io::Result<Self> {
        if start_ticks == 0 {
            return Err(io::Error::other("invalid parent start"));
        }
        let parent = crate::process_identity::Pinned::capture(pid)?;
        if parent.identity().start_ticks != start_ticks {
            return Err(io::Error::other("parent lifetime mismatch"));
        }
        Ok(Self(parent))
    }
    pub fn from_environment() -> io::Result<Self> {
        fn counter(name: &str) -> io::Result<u64> {
            let value = std::env::var(name).map_err(io::Error::other)?;
            if value.is_empty()
                || value.starts_with('0')
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(io::Error::other("invalid parent environment"));
            }
            value.parse().map_err(io::Error::other)
        }
        let pid = u32::try_from(counter("OMARCHY_MODAL_PARENT_PID")?).map_err(io::Error::other)?;
        Self::pin(pid, counter("OMARCHY_MODAL_PARENT_START_TICKS")?)
    }
    pub fn check(&self) -> io::Result<()> {
        self.0.check()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    version: u8,
    namespace: String,
    authority_epoch: [u8; 16],
    generation: String,
    client_id: [u8; 16],
    client_epoch: String,
    state: String,
}
impl Hello {
    fn new(namespace: String, fence: Fence) -> Self {
        Self {
            version: 1,
            namespace,
            authority_epoch: fence.authority_epoch.0,
            generation: fence.generation.to_string(),
            client_id: fence.owner.client.0,
            client_epoch: fence.owner.client_epoch.to_string(),
            state: "mapped".into(),
        }
    }
}
pub struct Channel {
    listener: UnixListener,
    _directory: File,
    path: PathBuf,
    external: PathBuf,
    inode: (u64, u64),
    hello: Hello,
    accepted: bool,
    connection: Option<Connection>,
    activated: bool,
    parent: crate::process_identity::Identity,
}
impl Channel {
    pub fn bind(root: &Path, fence: Fence) -> io::Result<Self> {
        let parent = crate::process_identity::Pinned::capture(std::process::id())?.identity();
        let directory = directory(root)?;
        let nonce = crate::host::epoch()?
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let name = format!("ready-{nonce}.sock");
        let external = root.join(&name);
        if external.as_os_str().len() >= 104 {
            return Err(io::Error::other("readiness socket path too long"));
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
        let listener = UnixListener::bind(&path)?;
        let m = std::fs::symlink_metadata(&path)?;
        let channel = Self {
            listener,
            _directory: directory,
            path,
            external,
            inode: (m.dev(), m.ino()),
            hello: Hello::new(format!("omarchy-modal-{nonce}"), fence),
            accepted: false,
            connection: None,
            activated: false,
            parent,
        };
        std::fs::set_permissions(&channel.path, std::fs::Permissions::from_mode(0o600))?;
        channel.listener.set_nonblocking(true)?;
        Ok(channel)
    }
    /// Called only after independent compositor mapping proof for this fence.
    pub fn activate(&mut self, deadline: Instant) -> io::Result<()> {
        if !self.accepted || self.activated {
            return Err(io::Error::other("invalid activation state"));
        }
        let mut hello = self.hello.clone();
        hello.state = "active".into();
        self.connection
            .as_mut()
            .ok_or_else(|| io::Error::other("readiness connection absent"))?
            .write_frame(
                &serde_json::to_vec(&hello).expect("closed activation"),
                deadline,
            )?;
        self.activated = true;
        Ok(())
    }
    pub fn namespace(&self) -> &str {
        &self.hello.namespace
    }
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        vec![
            (
                "OMARCHY_MODAL_SOCKET".into(),
                self.external.clone().into_os_string(),
            ),
            ("OMARCHY_MODAL_NAMESPACE".into(), self.namespace().into()),
            (
                "OMARCHY_MODAL_READY".into(),
                serde_json::to_string(&self.hello)
                    .expect("closed hello")
                    .into(),
            ),
            (
                "OMARCHY_MODAL_PARENT_PID".into(),
                self.parent.pid.to_string().into(),
            ),
            (
                "OMARCHY_MODAL_PARENT_START_TICKS".into(),
                self.parent.start_ticks.to_string().into(),
            ),
        ]
    }
    /// SO_PEERCRED exact dedicated PID + UID, full fence and fresh namespace.
    /// Returns only a child report. Caller must independently query layers.
    pub fn receive(&mut self, expected_pid: u32, deadline: Instant) -> io::Result<()> {
        if self.accepted || expected_pid == 0 {
            return Err(io::Error::other("invalid readiness state"));
        }
        let mut attempts = 0;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    attempts += 1;
                    if attempts > 32 {
                        return Err(io::Error::other("readiness admission limit"));
                    }
                    let Ok(mut connection) = Connection::from_stream(stream) else {
                        continue;
                    };
                    if connection.pid as u32 != expected_pid {
                        continue;
                    }
                    let frame = connection.read_frame(deadline)?;
                    let hello: Hello = serde_json::from_slice(&frame)
                        .map_err(|_| io::Error::other("invalid readiness frame"))?;
                    if hello != self.hello {
                        return Err(io::Error::other("stale readiness frame"));
                    }
                    self.accepted = true;
                    self.connection = Some(connection);
                    return Ok(());
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(
                        Duration::from_millis(2)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    )
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "readiness deadline",
        ))
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
/// Child helper called after GTK mapping/commit. It authenticates the parent
/// listener, sends one bounded report and waits for a full-fence activation reply.
/// Only after Ok may the matching dedicated UI generation request keyboard focus.
/// Keep this off the GTK executor. Failure must close the dedicated presenter.
pub fn report_mapped(
    socket: &Path,
    parent_pid: u32,
    hello_json: &str,
    deadline: Instant,
) -> io::Result<()> {
    if hello_json.len() > 1024 || parent_pid == 0 || Instant::now() >= deadline {
        return Err(io::Error::other("invalid child readiness"));
    }
    let hello: Hello = serde_json::from_str(hello_json)
        .map_err(|_| io::Error::other("invalid child readiness"))?;
    if hello.version != 1
        || hello.state != "mapped"
        || !hello.namespace.starts_with("omarchy-modal-")
        || hello.namespace.len() != 46
    {
        return Err(io::Error::other("invalid child readiness"));
    }
    // Nonblocking connect via a short-lived raw socket avoids blocking a full backlog.
    let stream = connect(socket, deadline)?;
    let mut peer = Connection::from_stream(stream)?;
    if peer.pid as u32 != parent_pid {
        return Err(io::Error::other("wrong readiness parent"));
    }
    peer.write_frame(hello_json.as_bytes(), deadline)?;
    let frame = peer.read_frame(deadline)?;
    let reply: Hello =
        serde_json::from_slice(&frame).map_err(|_| io::Error::other("invalid activation reply"))?;
    let mut expected = hello;
    expected.state = "active".into();
    if reply != expected {
        return Err(io::Error::other("stale activation reply"));
    }
    Ok(())
}
fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    if !path.is_absolute()
        || bytes.is_empty()
        || bytes.len() >= addr.sun_path.len()
        || bytes.contains(&0)
    {
        return Err(io::Error::other("invalid socket path"));
    }
    for (a, b) in addr.sun_path.iter_mut().zip(bytes) {
        *a = *b as _;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "readiness connect"));
            }
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 0) } > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut error: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut libc::c_int).cast(),
                &mut size,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(UnixStream::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    fn fence() -> Fence {
        Fence {
            authority_epoch: modal_contract::AuthorityEpoch([4; 16]),
            generation: NonZeroU64::new(1).unwrap(),
            owner: modal_contract::Owner {
                client: modal_contract::ClientId([2; 16]),
                client_epoch: NonZeroU64::new(1).unwrap(),
            },
        }
    }
    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
    #[test]
    fn parent_watch_refuses_reused_identity_and_lost_parent() {
        let mut child = std::process::Command::new("/usr/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = crate::process_identity::Pinned::capture(child.id())
            .unwrap()
            .identity();
        let watch = ParentWatch::pin(identity.pid, identity.start_ticks).unwrap();
        assert!(ParentWatch::pin(identity.pid, identity.start_ticks + 1).is_err());
        watch.check().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(watch.check().is_err());
    }
    #[test]
    fn child_fixture() {
        let Ok(path) = std::env::var("OMARCHY_MODAL_SOCKET") else {
            return;
        };
        report_mapped(
            Path::new(&path),
            std::env::var("OMARCHY_MODAL_PARENT_PID")
                .unwrap()
                .parse()
                .unwrap(),
            &std::env::var("OMARCHY_MODAL_READY").unwrap(),
            deadline(),
        )
        .unwrap();
    }
    #[test]
    fn real_dedicated_child_authenticates_parent_and_reports_exact_fence() {
        let root = root();
        let mut channel = Channel::bind(root.path(), fence()).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "readiness::tests::child_fixture"])
            .env_clear()
            .envs(channel.environment())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        channel.receive(child.id(), deadline()).unwrap();
        channel.activate(deadline()).unwrap();
        assert!(channel.accepted);
        assert!(child.wait().unwrap().success());
        assert!(channel.receive(child.id(), deadline()).is_err());
    }
    #[test]
    fn wrong_child_pid_and_wrong_parent_pid_are_rejected() {
        let root = root();
        let mut channel = Channel::bind(root.path(), fence()).unwrap();
        let hello = serde_json::to_string(&channel.hello).unwrap();
        assert!(
            report_mapped(
                &channel.external,
                std::process::id() + 1,
                &hello,
                deadline()
            )
            .is_err()
        );
        {
            use std::io::Write;
            let mut connection = UnixStream::connect(&channel.external).unwrap();
            writeln!(connection, "{hello}").unwrap();
        }
        assert!(
            channel
                .receive(
                    std::process::id() + 1,
                    Instant::now() + Duration::from_millis(20)
                )
                .is_err()
        );
        assert!(!channel.accepted);
    }
    #[test]
    fn stale_fence_forged_namespace_and_unknown_fields_do_not_qualify() {
        use std::io::Write;
        for field in ["generation", "namespace", "extra"] {
            let root = root();
            let mut channel = Channel::bind(root.path(), fence()).unwrap();
            let mut frame = serde_json::to_value(&channel.hello).unwrap();
            frame[field] = serde_json::json!("wrong");
            let mut stream = UnixStream::connect(&channel.external).unwrap();
            writeln!(stream, "{frame}").unwrap();
            assert!(channel.receive(std::process::id(), deadline()).is_err());
            assert!(!channel.accepted);
        }
    }
    #[test]
    fn fresh_channel_namespace_and_inode_scoped_cleanup() {
        let root = root();
        let first = Channel::bind(root.path(), fence()).unwrap();
        let second = Channel::bind(root.path(), fence()).unwrap();
        assert_ne!(first.namespace(), second.namespace());
        let path = first.external.clone();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(first);
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }
    #[test]
    fn map_report_without_activation_never_enables_child() {
        let root = root();
        let mut channel = Channel::bind(root.path(), fence()).unwrap();
        let path = channel.external.clone();
        let hello = serde_json::to_string(&channel.hello).unwrap();
        std::thread::scope(|scope| {
            let child = scope.spawn(|| {
                report_mapped(
                    &path,
                    std::process::id(),
                    &hello,
                    Instant::now() + Duration::from_millis(40),
                )
            });
            channel.receive(std::process::id(), deadline()).unwrap();
            assert!(child.join().unwrap().is_err());
            assert!(!channel.activated);
        });
    }
    #[test]
    fn stale_activation_reply_is_rejected_by_child() {
        let root = root();
        let mut channel = Channel::bind(root.path(), fence()).unwrap();
        let path = channel.external.clone();
        let hello = serde_json::to_string(&channel.hello).unwrap();
        std::thread::scope(|scope| {
            let child =
                scope.spawn(|| report_mapped(&path, std::process::id(), &hello, deadline()));
            channel.receive(std::process::id(), deadline()).unwrap();
            let mut wrong = channel.hello.clone();
            wrong.state = "active".into();
            wrong.generation = "2".into();
            channel
                .connection
                .as_mut()
                .unwrap()
                .write_frame(&serde_json::to_vec(&wrong).unwrap(), deadline())
                .unwrap();
            assert!(child.join().unwrap().is_err());
            assert!(!channel.activated);
        });
    }
}
