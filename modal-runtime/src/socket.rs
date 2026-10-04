//! Private socket custody and bounded framing. Host owns admission/actor scheduling.
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_FRAME: usize = 4096;
pub struct ControlSocket {
    listener: UnixListener,
    _lock: File,
    _directory: File,
    path: PathBuf,
    inode: (u64, u64),
    preserve: bool,
}
pub struct Connection {
    stream: UnixStream,
    pub uid: u32,
    pub pid: i32,
    failed: bool,
}
pub(crate) fn directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute runtime required"));
    }
    let mut file = File::open("/")?;
    for part in path.components() {
        let Component::Normal(part) = part else {
            if part == Component::RootDir {
                continue;
            }
            return Err(io::Error::other("unsafe runtime component"));
        };
        let part = CString::new(part.as_bytes()).map_err(io::Error::other)?;
        let fd = unsafe {
            libc::openat(
                file.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    let m = file.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe runtime",
        ));
    }
    Ok(file)
}
impl ControlSocket {
    /// The caller creates the dedicated private runtime directory. Existing socket
    /// paths are refused, even stale ones: recovery must explicitly establish custody.
    pub fn bind(root: &Path) -> io::Result<Self> {
        let directory = directory(root)?;
        let base = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(base.join("modal.lock"))?;
        let m = lock.metadata()?;
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.nlink() != 1
            || m.mode() & 0o777 != 0o600
        {
            return Err(io::Error::other("unsafe lock"));
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let path = base.join("modal.sock");
        let listener = UnixListener::bind(&path)?;
        let m = std::fs::symlink_metadata(&path)?;
        let guard = Self {
            listener,
            _lock: lock,
            _directory: directory,
            path,
            inode: (m.dev(), m.ino()),
            preserve: false,
        };
        std::fs::set_permissions(&guard.path, std::fs::Permissions::from_mode(0o600))?;
        guard.listener.set_nonblocking(true)?;
        Ok(guard)
    }
    pub(crate) fn preserve_for_recovery(&mut self) {
        self.preserve = true;
    }
    pub fn accept(&self) -> io::Result<Connection> {
        let (stream, _) = self.listener.accept()?;
        Connection::from_stream(stream)
    }
}
impl Drop for ControlSocket {
    fn drop(&mut self) {
        if self.preserve {
            return;
        }
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn pause(deadline: Instant) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "frame deadline"));
    }
    std::thread::sleep(remaining.min(Duration::from_millis(2)));
    Ok(())
}
impl Connection {
    pub(crate) fn from_stream(stream: UnixStream) -> io::Result<Self> {
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if len as usize != std::mem::size_of::<libc::ucred>()
            || peer.uid != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "foreign peer",
            ));
        }
        stream.set_nonblocking(true)?;
        Ok(Connection {
            stream,
            uid: peer.uid,
            pid: peer.pid,
            failed: false,
        })
    }
    pub(crate) fn try_read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.read(bytes)
    }
    pub(crate) fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.write(bytes)
    }
    /// Nonblocking, nonconsuming readiness hint for an event-loop owner.
    ///
    /// `true` includes closure/error readiness: call `read_frame` with a bounded
    /// absolute deadline to resolve it. It does not promise a complete frame or
    /// authenticate the peer's current process identity. Idle input is `false`,
    /// never an implicit release or a reason to restart an existing frame budget.
    pub fn read_ready(&mut self) -> io::Result<bool> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection failed",
            ));
        }
        let mut descriptor = libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // The stream owns this descriptor throughout the call. Zero timeout
        // probes once; EINTR is returned instead of an unbounded retry loop.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            self.failed = true;
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "invalid socket"));
        }
        Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
    }
    /// Absolute deadline applies to the whole frame, including byte-drip peers.
    /// Newline framing deliberately leaves subsequent frames on the socket.
    pub fn read_frame(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection failed",
            ));
        }
        let result = self.read_inner(deadline);
        if result.is_err() {
            self.failed = true;
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
        result
    }
    fn read_inner(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "frame deadline"));
            }
            let mut byte = [0];
            match self.stream.read(&mut byte) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed frame")),
                Ok(_) => {
                    if byte[0] == b'\n' {
                        return Ok(bytes);
                    }
                    if bytes.len() == MAX_FRAME {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame budget"));
                    }
                    bytes.push(byte[0]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
    pub fn write_frame(&mut self, data: &[u8], deadline: Instant) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection failed",
            ));
        }
        let result = self.write_inner(data, deadline);
        if result.is_err() {
            self.failed = true;
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
        result
    }
    fn write_inner(&mut self, data: &[u8], deadline: Instant) -> io::Result<()> {
        if data.len() > MAX_FRAME || data.contains(&b'\n') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame budget"));
        }
        let mut bytes = data.to_vec();
        bytes.push(b'\n');
        let mut offset = 0;
        while offset < bytes.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "frame deadline"));
            }
            match self.stream.write(&bytes[offset..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "closed writer")),
                Ok(n) => offset += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn singleton_excludes_an_independent_process() {
        if let Some(root) = std::env::var_os("OMARCHY_MODAL_TEST_LOCK_ROOT") {
            assert!(ControlSocket::bind(Path::new(&root)).is_err());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let _server = ControlSocket::bind(dir.path()).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "socket::tests::singleton_excludes_an_independent_process",
            ])
            .env("OMARCHY_MODAL_TEST_LOCK_ROOT", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn singleton_framing_peer_and_deadline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = ControlSocket::bind(dir.path()).unwrap();
        assert!(ControlSocket::bind(dir.path()).is_err());
        let mut client = UnixStream::connect(dir.path().join("modal.sock")).unwrap();
        let mut peer = server.accept().unwrap();
        assert_eq!(peer.uid, unsafe { libc::geteuid() });
        assert_eq!(peer.pid as u32, std::process::id());
        client.write_all(b"first\nsecond\n").unwrap();
        assert_eq!(
            peer.read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            b"first"
        );
        assert_eq!(
            peer.read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            b"second"
        );
        assert_eq!(
            peer.read_frame(Instant::now() + Duration::from_millis(10))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            peer.read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        let mut client = UnixStream::connect(dir.path().join("modal.sock")).unwrap();
        let mut peer = server.accept().unwrap();
        client.write_all(&vec![b'x'; MAX_FRAME + 1]).unwrap();
        assert_eq!(
            peer.read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        drop(server);
        assert!(!dir.path().join("modal.sock").exists());
        assert!(dir.path().join("modal.lock").exists());
    }
    #[test]
    fn readiness_preserves_idle_bytes_and_eof() {
        let (mut writer, stream) = UnixStream::pair().unwrap();
        let mut reader = Connection::from_stream(stream).unwrap();
        for _ in 0..16 {
            assert!(!reader.read_ready().unwrap(), "idle must not become input");
        }
        writer.write_all(b"first\nsecond\n").unwrap();
        assert!(reader.read_ready().unwrap(), "queued data must be ready");
        assert!(reader.read_ready().unwrap(), "probe must not consume data");
        assert_eq!(
            reader
                .read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            b"first"
        );
        assert!(reader.read_ready().unwrap());
        assert_eq!(
            reader
                .read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            b"second"
        );
        assert!(!reader.read_ready().unwrap());
        writer.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(reader.read_ready().unwrap(), "EOF must not look like idle");
        assert_eq!(
            reader
                .read_frame(Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            reader.read_ready().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn partial_readiness_does_not_extend_frame_deadline() {
        let (mut writer, stream) = UnixStream::pair().unwrap();
        let mut reader = Connection::from_stream(stream).unwrap();
        writer.write_all(b"partial").unwrap();
        assert!(reader.read_ready().unwrap());
        assert_eq!(
            reader
                .read_frame(Instant::now() + Duration::from_millis(10))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            reader.read_ready().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn unsafe_runtime_and_replaced_endpoint_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ControlSocket::bind(dir.path()).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = ControlSocket::bind(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("modal.sock")).unwrap();
        std::fs::write(dir.path().join("modal.sock"), b"preserve").unwrap();
        drop(server);
        assert_eq!(
            std::fs::read(dir.path().join("modal.sock")).unwrap(),
            b"preserve"
        );
    }
}
