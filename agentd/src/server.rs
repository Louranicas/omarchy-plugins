use crate::private_dir::PrivateDir;
use crate::procfs::ProcfsScanner;
use crate::protocol::{AckFrame, ErrorCode, ErrorFrame, Request, error_message, parse_request};
use crate::state::{ActivityError, NameError, SlotEvent, StateStore, Subscription, encode_frame};
use crate::{Clock, SystemClock};
use std::fs;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// Fixed transport budgets preserve the v1 wire format and CLI surface.
const MAX_CLIENTS: usize = 64;
const MAX_SUBSCRIBERS: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_REQUEST_BYTES: usize = 65_536;
const SOCKET_SEND_BUFFER_BYTES: libc::c_int = 65_536;
const SCAN_INTERVAL: Duration = Duration::from_millis(250);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigterm(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn socket_path() -> Result<PathBuf, String> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_owned())?;
    let runtime = PathBuf::from(runtime);
    PrivateDir::open(&runtime, true).map_err(|e| format!("unsafe XDG_RUNTIME_DIR: {e}"))?;
    Ok(runtime.join("agentd.sock"))
}

pub fn run_daemon() -> Result<(), String> {
    let path = socket_path()?;
    let scanner = ProcfsScanner::system();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    run_daemon_at(path, scanner, clock).map_err(|error| error.to_string())
}

pub fn run_daemon_at(
    path: PathBuf,
    scanner: ProcfsScanner,
    clock: Arc<dyn Clock>,
) -> io::Result<()> {
    let directory = PrivateDir::open(
        path.parent()
            .ok_or_else(|| io::Error::other("socket parent missing"))?,
        true,
    )?;
    let _singleton = directory.lock(".agentd.lock", Duration::ZERO)?;
    let path = directory.target(&path)?;
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    install_sigterm_handler().map_err(|error| {
        io::Error::new(error.kind(), format!("install SIGTERM handler: {error}"))
    })?;
    let store = Arc::new(StateStore::new().map_err(|error| {
        io::Error::new(error.kind(), format!("create daemon instance ID: {error}"))
    })?);
    let initial = scanner.scan(None, clock.as_ref());
    store.commit_scan(initial);
    let listener = bind_socket(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("bind socket {}: {error}", path.display()),
        )
    })?;
    let bound = fs::symlink_metadata(&path)?;
    // Run every fallible startup/serve step inside the cleanup boundary.
    let result = run_listener(listener, scanner, store, clock);
    let cleanup = remove_if_same_socket(&path, &bound);
    result.and(cleanup)
}

struct ClientWorker {
    socket: UnixStream,
    thread: thread::JoinHandle<()>,
}

fn run_listener(
    listener: UnixListener,
    scanner: ProcfsScanner,
    store: Arc<StateStore>,
    clock: Arc<dyn Clock>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let subscribers = Arc::new(AtomicUsize::new(0));
    let scanner_store = store.clone();
    let scanner_clock = clock.clone();
    let scanner_thread = thread::Builder::new()
        .name("agentd-scan".into())
        .spawn(move || {
            while !STOP_REQUESTED.load(Ordering::SeqCst) {
                thread::sleep(SCAN_INTERVAL);
                if STOP_REQUESTED.load(Ordering::SeqCst) {
                    break;
                }
                let previous = scanner_store.current_snapshot();
                let proposal = scanner.scan(previous.as_deref(), scanner_clock.as_ref());
                scanner_store.commit_scan(proposal);
            }
        })?;
    let mut workers: Vec<ClientWorker> = Vec::new();
    let result = (|| {
        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            // Reap on idle iterations too; completed clients cannot retain FDs/handles.
            let mut index = 0;
            while index < workers.len() {
                if workers[index].thread.is_finished() {
                    let worker = workers.swap_remove(index);
                    worker
                        .thread
                        .join()
                        .map_err(|_| io::Error::other("client thread panicked"))?;
                } else {
                    index += 1;
                }
            }
            if scanner_thread.is_finished() {
                return Err(io::Error::other("procfs scanner exited unexpectedly"));
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if !peer_uid_matches(&stream, unsafe { libc::geteuid() }).unwrap_or(false) {
                        drop(stream);
                        continue;
                    }
                    let deadline = Instant::now() + REQUEST_TIMEOUT;
                    if workers.len() >= MAX_CLIENTS {
                        // Reject without allocating a thread or writing to an untrusted peer.
                        // No new error code is added to the closed v1 protocol.
                        drop(stream);
                        continue;
                    }
                    let socket = stream.try_clone()?;
                    let store = store.clone();
                    let clock = clock.clone();
                    let subscribers = subscribers.clone();
                    let handle =
                        thread::Builder::new()
                            .name("agentd-client".into())
                            .spawn(move || {
                                let _ =
                                    serve_connection(stream, store, clock, subscribers, deadline);
                            })?;
                    workers.push(ClientWorker {
                        socket,
                        thread: handle,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })();
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    drop(listener);
    // Wake readers and blocked writers before joining any worker. Subscriber waits
    // also observe STOP, including subscriptions created concurrently with this close.
    for worker in &workers {
        let _ = worker.socket.shutdown(Shutdown::Both);
    }
    store.close_subscribers();
    let mut join_error = None;
    for worker in workers {
        if worker.thread.join().is_err() {
            join_error = Some(io::Error::other("client thread panicked"));
        }
    }
    if scanner_thread.join().is_err() {
        join_error = Some(io::Error::other("procfs scanner thread panicked"));
    }
    result.and(join_error.map_or(Ok(()), Err))
}

struct SubscriberPermit(Arc<AtomicUsize>);

impl SubscriberPermit {
    fn acquire(count: Arc<AtomicUsize>) -> Option<Self> {
        let mut current = count.load(Ordering::Acquire);
        loop {
            if current >= MAX_SUBSCRIBERS {
                return None;
            }
            match count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        Some(Self(count))
    }
}

impl Drop for SubscriberPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn install_sigterm_handler() -> io::Result<()> {
    let handler = handle_sigterm as *const () as libc::sighandler_t;
    let result = unsafe { libc::signal(libc::SIGTERM, handler) };
    if result == libc::SIG_ERR {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn peer_uid_matches(stream: &UnixStream, expected: u32) -> io::Result<bool> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: correctly sized writable credential buffer and live socket descriptor.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("invalid peer credential length"));
    }
    Ok(credentials.uid == expected)
}

fn connect_probe(path: &Path) -> io::Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::other("socket path too long"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Nonblocking backlog pressure returns EAGAIN: never interpret this as stale.
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    for _ in 0..8 {
        match fs::symlink_metadata(path) {
            Ok(metadata) => inspect_existing_socket(path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match UnixListener::bind(path) {
            Ok(listener) => {
                let bound = fs::symlink_metadata(path)?;
                if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
                    let _ = remove_if_same_socket(path, &bound);
                    return Err(error);
                }
                return Ok(listener);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "socket collision budget exhausted",
    ))
}

fn inspect_existing_socket(path: &Path, first: &fs::Metadata) -> io::Result<()> {
    if !first.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to remove non-socket path {}", path.display()),
        ));
    }
    let effective_uid = unsafe { libc::geteuid() as u32 };
    if first.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to remove socket not owned by local user: {}",
                path.display()
            ),
        ));
    }
    match connect_probe(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("live listener already owns {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            let second = fs::symlink_metadata(path)?;
            if !second.file_type().is_socket()
                || second.uid() != effective_uid
                || second.dev() != first.dev()
                || second.ino() != first.ino()
            {
                return Err(io::Error::other(format!(
                    "socket changed during stale-path check: {}",
                    path.display()
                )));
            }
            fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot establish stale socket state for {}: {error}",
                path.display()
            ),
        )),
    }
}

fn remove_if_same_socket(path: &Path, bound: &fs::Metadata) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(current)
            if current.file_type().is_socket()
                && current.dev() == bound.dev()
                && current.ino() == bound.ino() =>
        {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::other(format!(
            "socket path changed before shutdown cleanup: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn serve_connection(
    mut stream: UnixStream,
    store: Arc<StateStore>,
    clock: Arc<dyn Clock>,
    subscribers: Arc<AtomicUsize>,
    deadline: Instant,
) -> io::Result<()> {
    configure_send_buffer(&stream)?;
    let bytes = match read_request(&mut stream, deadline) {
        Ok(bytes) => bytes,
        Err(code) => return write_error(&mut stream, code),
    };
    let request = match parse_request(&bytes) {
        Ok(request) => request,
        Err(code) => return write_error(&mut stream, code),
    };
    match request {
        Request::Snapshot => write_frame(&mut stream, store.snapshot_frame().as_slice()),
        Request::Subscribe => {
            let Some(_permit) = SubscriberPermit::acquire(subscribers) else {
                return Ok(());
            };
            serve_subscription(&mut stream, store.subscribe())
        }
        Request::Activity { agent, state } => {
            match store.apply_activity(agent, state, clock.as_ref()) {
                Ok(ack) => write_frame(
                    &mut stream,
                    &encode_frame(&AckFrame {
                        frame_type: "ack",
                        instance_id: &ack.instance_id,
                        revision: ack.revision,
                    }),
                ),
                Err(ActivityError::UnknownAgent) => {
                    write_error(&mut stream, ErrorCode::UnknownAgent)
                }
            }
        }
        Request::Name { agent, name } => match store.apply_name(agent, name) {
            Ok(ack) => write_frame(
                &mut stream,
                &encode_frame(&AckFrame {
                    frame_type: "ack",
                    instance_id: &ack.instance_id,
                    revision: ack.revision,
                }),
            ),
            Err(NameError::UnknownAgent) => write_error(&mut stream, ErrorCode::UnknownAgent),
            Err(NameError::StoreUnavailable) => {
                write_error(&mut stream, ErrorCode::NameStoreUnavailable)
            }
        },
    }
}

fn serve_subscription(stream: &mut UnixStream, subscription: Subscription) -> io::Result<()> {
    let Subscription { initial, slot } = subscription;
    let result = (|| {
        write_owned_frame(stream, initial)?;
        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            match slot.take_timeout(CLIENT_POLL_INTERVAL) {
                SlotEvent::Frame(frame) => write_owned_frame(stream, frame)?,
                SlotEvent::Closed => break,
                SlotEvent::Idle => {
                    // POLLHUP detects a fully closed peer even when no roster update
                    // arrives. A legitimate SHUT_WR (request EOF) alone is not HUP.
                    let mut pollfd = libc::pollfd {
                        fd: stream.as_raw_fd(),
                        events: 0,
                        revents: 0,
                    };
                    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
                    if result > 0
                        && pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                    {
                        break;
                    }
                    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
        }
        Ok(())
    })();
    slot.close();
    result
}

fn write_owned_frame(stream: &mut UnixStream, frame: Arc<Vec<u8>>) -> io::Result<()> {
    write_frame(stream, frame.as_slice())
}

fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    write_until(stream, bytes, Instant::now() + FRAME_WRITE_TIMEOUT)
}

fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "frame write deadline exceeded")
            })?;
        // MSG_DONTWAIT is per-call: unlike O_NONBLOCK it does not alter the
        // cancellation clone's shared file description. A blocking send may
        // outlive SO_SNDTIMEO while the peer continuously makes progress.
        let count = unsafe {
            libc::send(
                stream.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if count > 0 {
            bytes = &bytes[count as usize..];
            continue;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write returned zero",
            ));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                let mut descriptor = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // Round up sub-millisecond budgets; the next loop checks the
                // absolute deadline again even after signals or readiness.
                let timeout = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(libc::c_int::MAX as u128) as libc::c_int;
                let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            _ => return Err(error),
        }
    }
    Ok(())
}

fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, ErrorCode> {
    let mut object = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        // Recompute from the acceptance time, not the last byte: slow byte-drip
        // clients cannot indefinitely renew the request budget.
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ErrorCode::MalformedRequest)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| ErrorCode::MalformedRequest)?;
        let count = match stream.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ErrorCode::MalformedRequest),
        };
        if Instant::now() >= deadline {
            return Err(ErrorCode::MalformedRequest);
        }
        if count == 0 {
            return Err(ErrorCode::MalformedRequest);
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                return Ok(object);
            }
            if object.len() == MAX_REQUEST_BYTES {
                return Err(ErrorCode::RequestTooLarge);
            }
            object.push(*byte);
        }
    }
}

fn write_error(stream: &mut UnixStream, code: ErrorCode) -> io::Result<()> {
    write_frame(
        stream,
        &encode_frame(&ErrorFrame {
            frame_type: "error",
            code,
            message: error_message(code),
        }),
    )
}

fn configure_send_buffer(stream: &UnixStream) -> io::Result<()> {
    let value = SOCKET_SEND_BUFFER_BYTES;
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Weak;

    #[test]
    fn peer_credentials_are_kernel_observed_and_mismatch_denied() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(peer_uid_matches(&socket, uid).unwrap());
        assert!(!peer_uid_matches(&socket, uid.wrapping_add(1)).unwrap());
    }

    #[test]
    fn full_listener_backlog_is_not_stale_or_blocking() {
        let path = std::env::temp_dir().join(format!("agentd-backlog-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        // SAFETY: live socket descriptor; shrink the accept queue for the fixture.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let _first = UnixStream::connect(&path).unwrap();
        let _second = UnixStream::connect(&path).unwrap();
        let start = Instant::now();
        assert!(inspect_existing_socket(&path, &fs::symlink_metadata(&path).unwrap()).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(path.exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn request_deadline_is_absolute_despite_byte_drip() {
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let writer = thread::spawn(move || {
            for _ in 0..100 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        assert_eq!(
            read_request(&mut server, start + Duration::from_millis(180)),
            Err(ErrorCode::MalformedRequest)
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "byte drip renewed read timeout"
        );
        drop(server);
        writer.join().unwrap();
    }

    #[test]
    fn expired_request_is_rejected_even_when_complete_bytes_are_ready() {
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"{\"version\":1,\"op\":\"snapshot\"}\n")
            .unwrap();
        assert_eq!(
            read_request(&mut server, Instant::now()),
            Err(ErrorCode::MalformedRequest)
        );
    }

    #[test]
    fn frame_deadline_expires_for_nonreading_and_draining_peers() {
        for draining in [false, true] {
            let (mut server, mut peer) = UnixStream::pair().unwrap();
            configure_send_buffer(&server).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = stop.clone();
            let reader = thread::spawn(move || {
                peer.set_nonblocking(true).unwrap();
                let mut bytes = [0; 8192];
                while !reader_stop.load(Ordering::SeqCst) {
                    if draining {
                        let _ = peer.read(&mut bytes);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            let payload = vec![b'x'; 16 * 1024 * 1024];
            let start = Instant::now();
            let result = write_until(&mut server, &payload, start + Duration::from_millis(180));
            stop.store(true, Ordering::SeqCst);
            reader.join().unwrap();
            let error = result.unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "partial progress renewed write timeout"
            );
        }
    }

    #[test]
    fn subscription_permits_recover_after_drop_and_unwind() {
        let count = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| SubscriberPermit::acquire(count.clone()).unwrap())
            .collect();
        assert!(SubscriberPermit::acquire(count.clone()).is_none());
        drop(permits);
        assert_eq!(count.load(Ordering::Acquire), 0);
        let unwind_count = count.clone();
        let _ = std::panic::catch_unwind(move || {
            let _permit = SubscriberPermit::acquire(unwind_count).unwrap();
            panic!("injected client failure");
        });
        assert_eq!(count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn owned_initial_frame_is_released_when_its_write_completes() {
        let retained_current = Arc::new(b"initial\n".to_vec());
        let initial_lifetime: Weak<Vec<u8>> = Arc::downgrade(&retained_current);
        let subscriber_initial = retained_current.clone();
        assert_eq!(initial_lifetime.strong_count(), 2);

        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write_owned_frame(&mut writer, subscriber_initial).unwrap();
        let mut written = [0; 8];
        reader.read_exact(&mut written).unwrap();
        assert_eq!(&written, b"initial\n");
        assert_eq!(
            initial_lifetime.strong_count(),
            1,
            "subscriber retained the already-sent initial frame"
        );
    }
}
