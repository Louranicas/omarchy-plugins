//! Single-owner bounded fixture IPC; no compositor dispatch, implicit socket discovery or live install.
use crate::{Request, Response, Runtime};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
pub const MAX_REQUEST: usize = 16 * 1024;
pub const MAX_RESPONSE: usize = 2 * 1024 * 1024;
fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
fn permission() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut c: libc::ucred = unsafe { mem::zeroed() };
    let mut n = mem::size_of_val(&c) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut c as *mut libc::ucred).cast(),
            &mut n,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != mem::size_of::<libc::ucred>() || c.uid != uid() || c.pid <= 0 {
        return Err(permission());
    }
    Ok(c)
}
fn directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > 4096 {
        return Err(invalid());
    }
    let mut dir = File::open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(n) => {
                let n = CString::new(n.as_encoded_bytes()).map_err(|_| invalid())?;
                let raw = unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        n.as_ptr(),
                        libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                dir = unsafe { File::from_raw_fd(raw) };
            }
            _ => return Err(invalid()),
        }
    }
    let m = dir.metadata()?;
    if m.uid() != uid() || m.mode() & 0o777 != 0o700 {
        return Err(permission());
    }
    Ok(dir)
}
fn read_frame(stream: &mut UnixStream, limit: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut b = [0; 4096];
    loop {
        let remain = deadline
            .checked_duration_since(Instant::now())
            .ok_or(io::ErrorKind::TimedOut)?;
        stream.set_read_timeout(Some(remain))?;
        let n = stream.read(&mut b)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if bytes.len() + n > limit {
            return Err(invalid());
        }
        bytes.extend_from_slice(&b[..n]);
        if let Some(i) = bytes.iter().position(|b| *b == b'\n') {
            if i + 1 != bytes.len() {
                return Err(invalid());
            }
            return Ok(bytes);
        }
    }
}
fn write_frame<T: serde::Serialize>(
    stream: &mut UnixStream,
    value: &T,
    limit: usize,
    deadline: Instant,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
    bytes.push(b'\n');
    if bytes.len() > limit {
        return Err(invalid());
    }
    let mut data = bytes.as_slice();
    while !data.is_empty() {
        stream.set_write_timeout(Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or(io::ErrorKind::TimedOut)?,
        ))?;
        let n = stream.write(data)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[n..];
    }
    Ok(())
}
pub struct Server {
    _directory: File,
    _lock: File,
    listener: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
    requests: BTreeMap<(i32, u64), u64>,
    pub runtime: Runtime,
}
impl Server {
    pub fn bind(folder: &Path, runtime: Runtime) -> io::Result<Self> {
        let directory = directory(folder)?;
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(pinned.join(".yoohoo.lock"))?;
        let m = lock.metadata()?;
        if !m.is_file() || m.uid() != uid() || m.mode() & 0o777 != 0o600 || m.nlink() != 1 {
            return Err(permission());
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let path = pinned.join("yoohoo.sock");
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let m = fs::symlink_metadata(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            _directory: directory,
            _lock: lock,
            listener,
            path,
            identity: (m.dev(), m.ino()),
            requests: BTreeMap::new(),
            runtime,
        })
    }
    pub fn serve_until(&mut self, lifetime: Duration) -> io::Result<()> {
        if lifetime.is_zero() || lifetime > Duration::from_secs(60) {
            return Err(invalid());
        }
        let started = Instant::now();
        let deadline = started + lifetime;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = self.handle(
                        &mut stream,
                        deadline,
                        started.elapsed().as_millis().min(u64::MAX as u128 - 10) as u64 + 10,
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn handle(&mut self, stream: &mut UnixStream, deadline: Instant, now: u64) -> io::Result<()> {
        let peer = credentials(stream)?;
        let mut stat = String::new();
        File::open(format!("/proc/{}/stat", peer.pid))?
            .take(8193)
            .read_to_string(&mut stat)?;
        if stat.len() > 8192 {
            return Err(invalid());
        }
        let close = stat.rfind(')').ok_or_else(invalid)?;
        let start: u64 = stat[close + 1..]
            .split_ascii_whitespace()
            .nth(19)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let deadline = deadline.min(Instant::now() + Duration::from_millis(250));
        let bytes = read_frame(stream, MAX_REQUEST, deadline)?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let owner = (peer.pid, start);
        if request.version != 1
            || request.request_id == 0
            || self
                .requests
                .get(&owner)
                .is_some_and(|last| request.request_id <= *last)
        {
            return Err(invalid());
        }
        if !self.requests.contains_key(&owner) && self.requests.len() >= 64 {
            return Err(permission());
        }
        self.requests.insert(owner, request.request_id);
        let response = self.runtime.handle(request, now);
        write_frame(stream, &response, MAX_RESPONSE, deadline)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
/// Fixture client: caller owns the daemon child lifetime and supplies its actual PID.
pub fn call(path: &Path, expected_pid: u32, request: Request) -> io::Result<Response> {
    if expected_pid == 0 || expected_pid > i32::MAX as u32 {
        return Err(invalid());
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    let parent = directory(path.parent().ok_or_else(invalid)?)?;
    if path.file_name() != Some(std::ffi::OsStr::new("yoohoo.sock")) {
        return Err(invalid());
    }
    let pinned = format!("/proc/self/fd/{}/yoohoo.sock", parent.as_raw_fd());
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
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    if pinned.len() >= address.sun_path.len() {
        return Err(invalid());
    }
    for (a, b) in address.sun_path.iter_mut().zip(pinned.bytes()) {
        *a = b as _;
    }
    let length =
        (mem::offset_of!(libc::sockaddr_un, sun_path) + pinned.len() + 1) as libc::socklen_t;
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    let peer = credentials(&stream)?;
    if peer.pid as u32 != expected_pid {
        return Err(permission());
    }
    let id = request.request_id;
    write_frame(&mut stream, &request, MAX_REQUEST, deadline)?;
    let bytes = read_frame(&mut stream, MAX_RESPONSE, deadline)?;
    let response: Response = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if response.request_id != id || response.view.rows.len() > 100 {
        return Err(invalid());
    }
    Ok(response)
}
