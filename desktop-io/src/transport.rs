use crate::{Error, MAX_EVENT, MAX_RESPONSE, ProcessIdentity, peer::Peer};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
static STREAM_ID: AtomicU64 = AtomicU64::new(1);
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
/// Explicitly selected from the target's reviewed configuration backend.
#[derive(Debug, Clone, Copy)]
pub enum DispatchDialect {
    Legacy,
    Lua,
}
/// Closed names only; the arbiter owns authorization and readback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalSubmap {
    Vimarchy,
    VimarchyDouble,
    Ask,
    Reset,
}
impl ModalSubmap {
    fn name(self) -> &'static str {
        match self {
            Self::Vimarchy => "vimarchy",
            Self::VimarchyDouble => "vimarchy-double",
            Self::Ask => "omarchy-ask",
            Self::Reset => "reset",
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum Query {
    Submap,
    Layers,
    Clients,
    Monitors,
    Workspaces,
    ActiveWindow,
    ActiveWorkspace,
    Version,
}
impl Query {
    fn wire(self) -> &'static [u8] {
        match self {
            Self::Clients => b"j/clients",
            Self::Monitors => b"j/monitors",
            Self::Workspaces => b"j/workspaces",
            Self::ActiveWindow => b"j/activewindow",
            Self::ActiveWorkspace => b"j/activeworkspace",
            Self::Version => b"j/version",
            Self::Layers => b"j/layers",
            Self::Submap => b"j/submap",
        }
    }
}
/// Authenticates UID/PID plus a retained pidfd lifetime and observed procfs start time.
/// Executable identity remains the responsibility of trusted compositor discovery.
pub struct Endpoint {
    directory: File,
    instance: String,
    uid: u32,
    peer: Arc<Peer>,
}
impl Endpoint {
    /// runtime must be absolute, owned by this user, and mode 0700.
    /// Every component is opened without following symlinks; hypr/HIS must be owned,
    /// non-group/world-writable directories. Filesystem calls are not cancellable.
    pub fn discover(runtime: &Path, instance: &str, expected_pid: u32) -> Result<Self, Error> {
        if expected_pid == 0
            || expected_pid > i32::MAX as u32
            || instance.is_empty()
            || instance.len() > 128
            || !instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::Invalid);
        }
        if !runtime.is_absolute() {
            return Err(Error::Invalid);
        }
        let mut dir = File::open("/")?;
        for component in runtime.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let name = CString::new(name.as_encoded_bytes()).map_err(|_| Error::Invalid)?;
                    dir = open_dir(&dir, &name)?;
                }
                _ => return Err(Error::Invalid),
            }
        }
        // SAFETY: geteuid has no arguments or memory requirements.
        let uid = unsafe { libc::geteuid() };
        check_dir(&dir, uid, true)?;
        dir = open_dir(&dir, c"hypr")?;
        check_dir(&dir, uid, false)?;
        dir = open_dir(&dir, &CString::new(instance).map_err(|_| Error::Invalid)?)?;
        check_dir(&dir, uid, false)?;
        let peer = Arc::new(Peer::pin(expected_pid, None)?);
        Ok(Self {
            directory: dir,
            instance: instance.into(),
            uid,
            peer,
        })
    }
    pub fn discover_identity(
        runtime: &Path,
        instance: &str,
        expected: ProcessIdentity,
    ) -> Result<Self, Error> {
        let endpoint = Self::discover(runtime, instance, expected.pid)?;
        if endpoint.peer.identity != expected {
            return Err(Error::Unauthenticated);
        }
        endpoint.peer.check()?;
        Ok(endpoint)
    }
    pub(crate) fn owns(&self, events: &Events) -> bool {
        Arc::ptr_eq(&self.peer, &events.peer) && self.instance == events.instance
    }
    pub fn process_identity(&self) -> ProcessIdentity {
        self.peer.identity
    }
    fn connect(&self, name: &str, deadline: Instant) -> Result<UnixStream, Error> {
        self.peer.check()?;
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        let path = PathBuf::from(format!(
            "/proc/self/fd/{}/{name}",
            self.directory.as_raw_fd()
        ));
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.file_type().is_socket() || meta.uid() != self.uid {
            return Err(Error::Unauthenticated);
        }
        // SAFETY: socket returns a new owned descriptor or -1.
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: raw was successfully created and ownership is transferred exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_un/ucred are C structures valid when zero initialized.
        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() >= addr.sun_path.len() {
            return Err(Error::Limit);
        }
        for (a, b) in addr.sun_path.iter_mut().zip(bytes) {
            *a = *b as _;
        }
        let len =
            (mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
        // SAFETY: sockaddr pointer and length cover initialized memory; descriptor is live.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                len,
            )
        };
        if result < 0 {
            let e = std::io::Error::last_os_error();
            // UNIX EAGAIN means backlog full, not an in-progress connection.
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(e.into());
            }
            wait(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
            let mut error: libc::c_int = 0;
            let mut size = mem::size_of_val(&error) as libc::socklen_t;
            // SAFETY: output pointers and length match c_int.
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
                return Err(std::io::Error::last_os_error().into());
            }
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error).into());
            }
        }
        let mut credentials: libc::ucred = unsafe { mem::zeroed() };
        let mut size = mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: output points to a live ucred and length is its exact size.
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut size,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if size as usize != mem::size_of::<libc::ucred>()
            || credentials.uid != self.uid
            || credentials.pid != self.peer.identity.pid as i32
        {
            return Err(Error::Unauthenticated);
        }
        self.peer.check()?;
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        Ok(UnixStream::from(fd))
    }
    pub fn query(&self, query: Query, budget: Duration) -> Result<serde_json::Value, Error> {
        let response = self.exchange(query.wire(), deadline(budget)?, MAX_RESPONSE)?;
        crate::json::parse(&response)
    }
    /// Narrow compositor mutation. Arbiter caller MUST validate lease/fence, trusted
    /// registry target and current state before invoking, then read back the outcome.
    /// A successful "ok" acknowledges dispatch only, not verified focus.
    /// Target-specific existing numeric workspace move; no general Lua surface.
    pub fn move_to_workspace(
        &self,
        id: crate::StableId,
        destination: u8,
        follow: bool,
        dialect: DispatchDialect,
        deadline: Instant,
    ) -> Result<(), Error> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining > Duration::from_secs(5)
            || !(1..=10).contains(&destination)
            || !matches!(dialect, DispatchDialect::Lua)
        {
            return Err(Error::Invalid);
        }
        let wire = format!(
            "/dispatch hl.dsp.window.move({{workspace=\"{}\",follow={},window=\"stableid:{}\"}})",
            destination,
            follow,
            id.canonical()
        );
        let reply = self.exchange(wire.as_bytes(), deadline, 256)?;
        if reply == b"ok" {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    pub fn set_maximized(
        &self,
        id: crate::StableId,
        set: bool,
        dialect: DispatchDialect,
        deadline: Instant,
    ) -> Result<(), Error> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining > Duration::from_secs(5) || !matches!(dialect, DispatchDialect::Lua) {
            return Err(Error::Invalid);
        }
        let action = if set { "set" } else { "unset" };
        let wire = format!(
            "/dispatch hl.dsp.window.fullscreen({{mode=\"maximized\",action=\"{}\",layout_aware=false,window=\"stableid:{}\"}})",
            action,
            id.canonical()
        );
        let response = self.exchange(wire.as_bytes(), deadline, 256)?;
        if response == b"ok" {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    pub fn focus_stable_id(
        &self,
        id: crate::StableId,
        dialect: DispatchDialect,
        deadline: Instant,
    ) -> Result<(), Error> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining > Duration::from_secs(5) {
            return Err(Error::Invalid);
        }
        let wire = match dialect {
            DispatchDialect::Legacy => format!("/dispatch focuswindow stableid:{}", id.canonical()),
            DispatchDialect::Lua => format!(
                "/dispatch hl.dsp.focus({{window=\"stableid:{}\"}})",
                id.canonical()
            ),
        };
        let response = self.exchange(wire.as_bytes(), deadline, 256)?;
        if response == b"ok" {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    /// Closed modal transition. Dispatch acknowledgment is not readiness or
    /// ownership proof; only the arbiter may authorize and verify this operation.
    pub fn set_modal_submap(
        &self,
        submap: ModalSubmap,
        dialect: DispatchDialect,
        deadline: Instant,
    ) -> Result<(), Error> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining > Duration::from_secs(5) {
            return Err(Error::Invalid);
        }
        let wire = match dialect {
            DispatchDialect::Legacy => format!("/dispatch submap {}", submap.name()),
            DispatchDialect::Lua => format!("/dispatch hl.dsp.submap(\"{}\")", submap.name()),
        };
        if self.exchange(wire.as_bytes(), deadline, 256)? == b"ok" {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    fn exchange(
        &self,
        mut request: &[u8],
        deadline: Instant,
        response_limit: usize,
    ) -> Result<Vec<u8>, Error> {
        let mut socket = self.connect(".socket.sock", deadline)?;
        while !request.is_empty() {
            wait(socket.as_raw_fd(), libc::POLLOUT, deadline)?;
            match socket.write(request) {
                Ok(0) => return Err(Error::Disconnected),
                Ok(n) => request = &request[n..],
                Err(e) if retry(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let mut response = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            wait(socket.as_raw_fd(), libc::POLLIN, deadline)?;
            match socket.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if response.len() + n > response_limit {
                        return Err(Error::Limit);
                    }
                    response.extend_from_slice(&buf[..n]);
                }
                Err(e) if retry(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.peer.check()?;
        Ok(response)
    }
    pub fn events(&self, budget: Duration) -> Result<Events, Error> {
        let stream_id = STREAM_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Exhausted)?;
        Ok(Events {
            stream_id,
            instance: self.instance.clone(),
            socket: self.connect(".socket2.sock", deadline(budget)?)?,
            peer: self.peer.clone(),
            pending: Vec::new(),
            alive: true,
        })
    }
}
fn check_dir(dir: &File, uid: u32, private: bool) -> Result<(), Error> {
    let m = dir.metadata()?;
    if !m.is_dir()
        || m.uid() != uid
        || m.mode() & 0o022 != 0
        || (private && m.mode() & 0o777 != 0o700)
    {
        return Err(Error::Unauthenticated);
    }
    Ok(())
}
fn open_dir(parent: &File, name: &std::ffi::CStr) -> Result<File, Error> {
    // SAFETY: name is NUL terminated, parent descriptor remains open for the call.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: newly opened descriptor ownership transferred exactly once.
    Ok(unsafe { File::from_raw_fd(raw) })
}
fn deadline(budget: Duration) -> Result<Instant, Error> {
    if budget.is_zero() || budget > Duration::from_secs(5) {
        return Err(Error::Invalid);
    }
    Instant::now().checked_add(budget).ok_or(Error::Invalid)
}
fn retry(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
fn wait(fd: i32, events: i16, deadline: Instant) -> Result<(), Error> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        let timeout = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut p = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one initialized pollfd provided with matching count.
        let result = unsafe { libc::poll(&mut p, 1, timeout) };
        if result < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if result == 0 {
            return Err(Error::Deadline);
        }
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        if p.revents & libc::POLLNVAL != 0 {
            return Err(Error::Disconnected);
        }
        if p.revents & (events | libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(());
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub payload: String,
}
impl Event {
    pub fn parse(line: &[u8]) -> Result<Self, Error> {
        if line.len() > MAX_EVENT {
            return Err(Error::Limit);
        }
        let line = std::str::from_utf8(line).map_err(|_| Error::Invalid)?;
        let (name, payload) = line.split_once(">>").ok_or(Error::Invalid)?;
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || payload.contains(['\0', '\n', '\r'])
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            name: name.into(),
            payload: payload.into(),
        })
    }
}
pub struct Events {
    stream_id: u64,
    instance: String,
    socket: UnixStream,
    peer: Arc<Peer>,
    pending: Vec<u8>,
    alive: bool,
}
impl Events {
    pub(crate) fn stream_id(&self) -> u64 {
        self.stream_id
    }
    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }
    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty() && !self.pending.contains(&b'\n')
    }
    /// Nonblocking one-event poll. Partial frames remain bounded; callers must not
    /// accept a snapshot while has_partial() is true.
    pub fn poll_event(&mut self) -> Result<Option<Event>, Error> {
        if !self.alive {
            return Err(Error::Disconnected);
        }
        let result = self.poll_inner();
        if result.is_err() {
            self.alive = false;
            self.pending.clear();
        }
        result
    }
    fn poll_inner(&mut self) -> Result<Option<Event>, Error> {
        loop {
            self.peer.check()?;
            if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                let event = Event::parse(&self.pending[..index])?;
                self.pending.drain(..=index);
                return Ok(Some(event));
            }
            if self.pending.len() > MAX_EVENT {
                return Err(Error::Limit);
            }
            let mut bytes = [0; 4096];
            match self.socket.read(&mut bytes) {
                Ok(0) => return Err(Error::Disconnected),
                Ok(n) => self.pending.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// A timeout with no partial frame is retryable. Any other failure poisons the stream:
    /// caller must invalidate identities and reconnect/resnapshot, never silently skip.
    pub fn next_event(&mut self, budget: Duration) -> Result<Event, Error> {
        if !self.alive {
            return Err(Error::Disconnected);
        }
        let deadline = deadline(budget)?;
        let result = self.read_event(deadline);
        if result.is_err() && !(matches!(result, Err(Error::Deadline)) && self.pending.is_empty()) {
            self.alive = false;
            self.pending.clear();
        }
        result
    }
    fn read_event(&mut self, deadline: Instant) -> Result<Event, Error> {
        loop {
            self.peer.check()?;
            if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                let event = Event::parse(&self.pending[..index])?;
                self.pending.drain(..=index);
                return Ok(event);
            }
            if self.pending.len() > MAX_EVENT {
                return Err(Error::Limit);
            }
            wait(self.socket.as_raw_fd(), libc::POLLIN, deadline)?;
            let mut bytes = [0u8; 4096];
            match self.socket.read(&mut bytes) {
                Ok(0) => return Err(Error::Disconnected),
                Ok(n) => self.pending.extend_from_slice(&bytes[..n]),
                Err(e) if retry(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
