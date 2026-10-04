use desktop_io::ProcessIdentity;
use modal_runtime::process_identity::Pinned;
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, net::UnixStream},
    },
    path::{Component, Path},
    time::{Duration, Instant},
};

pub(super) struct Transport {
    stream: UnixStream,
    process: Pinned,
    _directory: File,
}
fn error() -> io::Error {
    io::Error::other("modal transport invalid or unavailable")
}
pub(crate) fn pin(identity: ProcessIdentity) -> io::Result<Pinned> {
    let process = Pinned::capture(identity.pid)?;
    if process.identity().start_ticks != identity.start_ticks {
        return Err(error());
    }
    process.check()?;
    Ok(process)
}
fn directory(root: &Path) -> io::Result<File> {
    if !root.is_absolute() || root.as_os_str().len() > 4096 {
        return Err(error());
    }
    let raw = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fd = unsafe { OwnedFd::from_raw_fd(raw) };
    for component in root.components() {
        let Component::Normal(name) = component else {
            if component == Component::RootDir {
                continue;
            }
            return Err(error());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| error())?;
        let raw = unsafe {
            libc::openat(
                fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        fd = unsafe { OwnedFd::from_raw_fd(raw) };
    }
    let file = File::from(fd);
    let m = file.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err(error());
    }
    Ok(file)
}
fn pause(deadline: Instant) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "modal deadline"));
    }
    std::thread::sleep(remaining.min(Duration::from_millis(1)));
    Ok(())
}
impl Transport {
    pub fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    pub fn connect(root: &Path, identity: ProcessIdentity) -> io::Result<Self> {
        if identity.pid == 0 || identity.pid > i32::MAX as u32 || identity.start_ticks == 0 {
            return Err(error());
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        let process = pin(identity)?;
        let directory = directory(root)?;
        let path = format!("/proc/self/fd/{}/modal.sock", directory.as_raw_fd());
        let m = std::fs::symlink_metadata(&path)?;
        if m.mode() & libc::S_IFMT != libc::S_IFSOCK
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o777 != 0o600
        {
            return Err(error());
        }
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        if path.len() >= addr.sun_path.len() {
            return Err(error());
        }
        for (to, from) in addr.sun_path.iter_mut().zip(path.bytes()) {
            *to = from as _;
        }
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1) as _,
            )
        };
        if result < 0 {
            if io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error());
            }
            loop {
                process.check()?;
                let mut p = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut p, 1, 0) } > 0 {
                    break;
                }
                pause(deadline)?;
            }
            let mut code: libc::c_int = 0;
            let mut size = std::mem::size_of_val(&code) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut code as *mut libc::c_int).cast(),
                    &mut size,
                )
            } < 0
                || code != 0
            {
                return Err(error());
            }
        }
        if Instant::now() >= deadline {
            return Err(error());
        }
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut size,
            )
        } < 0
            || size as usize != std::mem::size_of_val(&peer)
            || peer.uid != unsafe { libc::geteuid() }
            || peer.pid as u32 != identity.pid
        {
            return Err(error());
        }
        process.check()?;
        Ok(Self {
            stream: UnixStream::from(fd),
            process,
            _directory: directory,
        })
    }
    pub fn exchange(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<Vec<u8>> {
        if bytes.len() > 4096 || bytes.contains(&b'\n') || Instant::now() >= deadline {
            return Err(error());
        }
        self.process.check()?;
        let mut frame = bytes.to_vec();
        frame.push(b'\n');
        let mut offset = 0;
        while offset < frame.len() {
            if Instant::now() >= deadline {
                return Err(error());
            }
            self.process.check()?;
            match self.stream.write(&frame[offset..]) {
                Ok(0) => return Err(error()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut frame = Vec::new();
        let mut bytes = [0; 512];
        loop {
            if Instant::now() >= deadline {
                return Err(error());
            }
            self.process.check()?;
            match self.stream.read(&mut bytes) {
                Ok(0) => return Err(error()),
                Ok(n) => {
                    if let Some(end) = bytes[..n].iter().position(|b| *b == b'\n') {
                        if end + 1 != n || frame.len() + end > 4096 {
                            return Err(error());
                        }
                        frame.extend_from_slice(&bytes[..end]);
                        self.process.check()?;
                        return Ok(frame);
                    }
                    if frame.len() + n > 4096 {
                        return Err(error());
                    }
                    frame.extend_from_slice(&bytes[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}
