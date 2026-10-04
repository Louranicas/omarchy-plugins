//! Bounded previews from revalidated scoped observations. Decoder workers receive
//! one private bounded copy, a read-only /usr, no home, no network, and resource caps.
use ask_files::{Identity, Match};
#[cfg(test)]
use omarchy_process_runner::run;
use omarchy_process_runner::{CleanupState, Request, run_retaining};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
pub const MAX_INPUT: usize = 16 * 1024 * 1024;
pub const MAX_TEXT_UTF16: usize = 24_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Busy,
    CleanupPending,
    CleanupUnverified,
    Stale,
    Unsafe,
    TooLarge,
    Io,
    Cancelled,
    DecoderUnavailable,
    DecoderFailed,
    MalformedImage,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Preview {
    Text {
        text: String,
        truncated: bool,
    },
    Rgba {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    Unsupported,
}

pub const MAX_CONCURRENT_PREVIEWS: usize = 2;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Admission<'a>(&'a AtomicUsize);
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn admit(counter: &AtomicUsize) -> Result<Admission<'_>, Error> {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_CONCURRENT_PREVIEWS).then_some(n + 1)
        })
        .map_err(|_| Error::Busy)?;
    Ok(Admission(counter))
}

fn open(parent: Option<&File>, name: &OsStr, directory: bool) -> Result<File, Error> {
    let name = CString::new(name.as_bytes()).map_err(|_| Error::Invalid)?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe {
        libc::openat(
            parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd),
            name.as_ptr(),
            flags,
        )
    };
    if fd < 0 {
        Err(Error::Unsafe)
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn identity(m: &fs::Metadata) -> Identity {
    Identity {
        device: m.dev(),
        inode: m.ino(),
    }
}
fn read_selection(selected: &Match, cancelled: &AtomicBool) -> Result<Vec<u8>, Error> {
    if !selected.root.is_absolute() {
        return Err(Error::Invalid);
    }
    let mut directory = open(None, OsStr::new("/"), true)?;
    for component in selected.root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = open(Some(&directory), name, true)?,
            _ => return Err(Error::Invalid),
        }
    }
    if identity(&directory.metadata().map_err(|_| Error::Io)?) != selected.root_identity {
        return Err(Error::Stale);
    }
    let relative = selected
        .path
        .strip_prefix(&selected.root)
        .map_err(|_| Error::Invalid)?;
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() || parts.len() > 128 {
        return Err(Error::Invalid);
    }
    let mut file = None;
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(Error::Invalid);
        };
        if index + 1 == parts.len() {
            file = Some(open(Some(&directory), name, false)?);
        } else {
            directory = open(Some(&directory), name, true)?;
        }
    }
    let mut file = file.ok_or(Error::Invalid)?;
    let before = file.metadata().map_err(|_| Error::Io)?;
    if !before.is_file() {
        return Err(Error::Unsafe);
    }
    if identity(&before) != selected.identity {
        return Err(Error::Stale);
    }
    if before.len() > MAX_INPUT as u64 {
        return Err(Error::TooLarge);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let n = file.read(&mut chunk).map_err(|_| Error::Io)?;
        if n == 0 {
            break;
        }
        if bytes.len() + n > MAX_INPUT {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let after = file.metadata().map_err(|_| Error::Io)?;
    if identity(&after) != identity(&before)
        || after.len() != before.len()
        || bytes.len() as u64 != before.len()
        || (
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) != (
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        )
    {
        return Err(Error::Stale);
    }
    Ok(bytes)
}

fn sandbox_request(copy: &Path, decoder: &str, args: &[&str]) -> Result<Request, Error> {
    // RLIMIT_NPROC exempts real UID 0: this boundary must fail closed for root.
    if unsafe { libc::getuid() } == 0 || unsafe { libc::geteuid() } == 0 {
        return Err(Error::Unsafe);
    }
    for path in [
        "/usr/bin/prlimit",
        "/usr/bin/bwrap",
        "/usr/bin/setpriv",
        decoder,
    ] {
        if !Path::new(path).is_file() {
            return Err(Error::DecoderUnavailable);
        }
    }
    let mut arguments: Vec<OsString> = [
        "--as=536870912",
        "--cpu=2",
        "--fsize=16777216",
        "--nofile=64",
        "--",
        "/usr/bin/bwrap",
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--setenv",
        "MAGICK_THREAD_LIMIT",
        "1",
        "--setenv",
        "OMP_NUM_THREADS",
        "1",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--size",
        "16777216",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    arguments.push(copy.as_os_str().to_owned());
    arguments.extend(
        [
            "/input",
            "--chdir",
            "/tmp",
            "--",
            "/usr/bin/setpriv",
            "--no-new-privs",
            "/usr/bin/prlimit",
            "--nproc=0:0",
            "--",
            decoder,
        ]
        .into_iter()
        .map(OsString::from),
    );
    arguments.extend(args.iter().map(OsString::from));
    Ok(Request {
        program: "/usr/bin/prlimit".into(),
        arguments,
        directory: PathBuf::from("/"),
        environment: vec![],
        timeout: Duration::from_secs(3),
        output_limit: 4 * 1024 * 1024,
    })
}

fn ppm(bytes: &[u8]) -> Result<Preview, Error> {
    // Accept the narrow P6 form emitted by pdftoppm; no general image decoder.
    let mut at = 0;
    let mut tokens = Vec::new();
    for _ in 0..4 {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            at += 1;
            if at - start > 16 {
                return Err(Error::MalformedImage);
            }
        }
        if start == at {
            return Err(Error::MalformedImage);
        }
        tokens.push(std::str::from_utf8(&bytes[start..at]).map_err(|_| Error::MalformedImage)?);
    }
    if tokens[0] != "P6" || tokens[3] != "255" || bytes.get(at) != Some(&b'\n') {
        return Err(Error::MalformedImage);
    }
    at += 1;
    let width: u32 = tokens[1].parse().map_err(|_| Error::MalformedImage)?;
    let height: u32 = tokens[2].parse().map_err(|_| Error::MalformedImage)?;
    if width == 0
        || height == 0
        || width > 512
        || height > 512
        || bytes.len() - at != (width * height * 3) as usize
    {
        return Err(Error::MalformedImage);
    }
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for rgb in bytes[at..].as_chunks::<3>().0 {
        pixels.extend_from_slice(rgb);
        pixels.push(255);
    }
    Ok(Preview::Rgba {
        width,
        height,
        pixels,
    })
}

pub fn preview(selected: &Match, cancelled: &AtomicBool) -> Result<Preview, Error> {
    preview_with_quota(selected, cancelled, &ACTIVE)
}

fn preview_with_quota(
    selected: &Match,
    cancelled: &AtomicBool,
    active: &'static AtomicUsize,
) -> Result<Preview, Error> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let _admission = admit(active)?;
    let bytes = read_selection(selected, cancelled)?;
    let image = bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"));
    let pdf = bytes.starts_with(b"%PDF-");
    if !image && !pdf {
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(Preview::Unsupported);
        };
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\0'))
        {
            return Ok(Preview::Unsupported);
        }
        let (mut answer, mut units, mut truncated) = (String::new(), 0, false);
        for c in text.chars().filter(|c| *c != '\0') {
            if units + c.len_utf16() > MAX_TEXT_UTF16 {
                truncated = true;
                break;
            }
            units += c.len_utf16();
            answer.push(c);
        }
        return Ok(Preview::Text {
            text: answer,
            truncated,
        });
    }
    // The private copy avoids granting the decoder access to the selected root.
    // It is removed on success/error/drop and is never a persistent preview cache.
    let temporary = tempfile::tempdir().map_err(|_| Error::Io)?;
    let input = temporary.path().join("input");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&input)
        .map_err(|_| Error::Io)?;
    file.write_all(&bytes).map_err(|_| Error::Io)?;
    drop(file);
    let request = if pdf {
        sandbox_request(
            &input,
            "/usr/bin/pdftoppm",
            &["-f", "1", "-singlefile", "-scale-to", "512", "/input"],
        )?
    } else {
        sandbox_request(
            &input,
            "/usr/bin/magick",
            &[
                "/input[0]",
                "-thumbnail",
                "512x512>",
                "-background",
                "none",
                "-gravity",
                "center",
                "-extent",
                "512x512",
                "-depth",
                "8",
                "RGBA:-",
            ],
        )?
    };
    // Quota and private input remain owned until actual child reap, including
    // timeout/cancellation. A still-running decoder cannot regain a freed slot.
    let output = run_retaining(&request, cancelled, (_admission, temporary)).map_err(
        |failure| match failure.cleanup.state() {
            CleanupState::Outstanding | CleanupState::Running => Error::CleanupPending,
            CleanupState::CustodyLost => Error::CleanupUnverified,
            CleanupState::NotSpawned | CleanupState::Reaped => {
                if failure.reason == omarchy_process_runner::Error::Cancelled {
                    Error::Cancelled
                } else {
                    Error::DecoderFailed
                }
            }
        },
    )?;

    if !output.status.success() {
        return Err(Error::DecoderFailed);
    }
    if pdf {
        return ppm(&output.stdout);
    }
    if output.stdout.len() != 512 * 512 * 4 {
        return Err(Error::MalformedImage);
    }
    Ok(Preview::Rgba {
        width: 512,
        height: 512,
        pixels: output.stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_image_and_pdf_decoders_return_bounded_raw_pixels() {
        static QUOTA: AtomicUsize = AtomicUsize::new(0);
        let preview = |selected: &Match, cancelled: &AtomicBool| {
            preview_with_quota(selected, cancelled, &QUOTA)
        };
        let t = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            (
                "image",
                include_bytes!("../tests/fixtures/red-pixel.png").as_slice(),
            ),
            (
                "pdf",
                include_bytes!("../tests/fixtures/blank.pdf").as_slice(),
            ),
        ] {
            let path = t.path().join(name);
            fs::write(&path, bytes).unwrap();
            let Preview::Rgba {
                width,
                height,
                pixels,
            } = preview(&selection(t.path(), &path), &AtomicBool::new(false)).unwrap()
            else {
                panic!("expected pixels")
            };
            assert!(width > 0 && height > 0 && width <= 512 && height <= 512);
            assert_eq!(pixels.len(), (width * height * 4) as usize);
        }
    }
    #[test]
    fn malformed_pixel_envelopes_rejected() {
        for bytes in [
            b"P6\n99999999 2\n255\n".as_slice(),
            b"P6\n1 1\n255\nxx",
            b"P6\n0 0\n255\n",
        ] {
            assert_eq!(ppm(bytes), Err(Error::MalformedImage));
        }
        assert_eq!(
            ppm(b"P6\n1 1\n255\n\x20\x0a\xff").unwrap(),
            Preview::Rgba {
                width: 1,
                height: 1,
                pixels: vec![32, 10, 255, 255]
            }
        );
    }
    fn selection(root: &Path, path: &Path) -> Match {
        Match {
            score: 0,
            path: path.into(),
            root: root.into(),
            root_identity: identity(&fs::metadata(root).unwrap()),
            identity: identity(&fs::metadata(path).unwrap()),
        }
    }
    #[test]
    fn text_bounds_utf16_and_strips_nul() {
        static QUOTA: AtomicUsize = AtomicUsize::new(0);
        let preview = |selected: &Match, cancelled: &AtomicBool| {
            preview_with_quota(selected, cancelled, &QUOTA)
        };
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("text");
        fs::write(&p, format!("hello\0{}", "😀".repeat(13000))).unwrap();
        let r = preview(&selection(t.path(), &p), &AtomicBool::new(false)).unwrap();
        let Preview::Text { text, truncated } = r else {
            panic!()
        };
        assert!(truncated);
        assert!(!text.contains('\0'));
        assert!(text.encode_utf16().count() <= 24000);
    }
    #[test]
    fn selection_replacement_and_symlink_escape_refused() {
        static QUOTA: AtomicUsize = AtomicUsize::new(0);
        let preview = |selected: &Match, cancelled: &AtomicBool| {
            preview_with_quota(selected, cancelled, &QUOTA)
        };
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("selected");
        fs::write(&p, b"before").unwrap();
        let s = selection(t.path(), &p);
        fs::rename(&p, t.path().join("original")).unwrap();
        fs::write(&p, b"after").unwrap();
        assert_eq!(preview(&s, &AtomicBool::new(false)), Err(Error::Stale));
        fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &p).unwrap();
        assert_eq!(preview(&s, &AtomicBool::new(false)), Err(Error::Unsafe));
    }
    #[test]
    fn oversized_and_binary_are_not_unbounded_text() {
        static QUOTA: AtomicUsize = AtomicUsize::new(0);
        let preview = |selected: &Match, cancelled: &AtomicBool| {
            preview_with_quota(selected, cancelled, &QUOTA)
        };
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("data");
        File::create(&p)
            .unwrap()
            .set_len(MAX_INPUT as u64 + 1)
            .unwrap();
        assert_eq!(
            preview(&selection(t.path(), &p), &AtomicBool::new(false)),
            Err(Error::TooLarge)
        );
        fs::write(&p, [0xff, 0xfe, 0xfd]).unwrap();
        assert_eq!(
            preview(&selection(t.path(), &p), &AtomicBool::new(false)).unwrap(),
            Preview::Unsupported
        );
    }
    #[test]
    fn decoder_plan_has_no_home_network_or_writable_host_bind() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("input");
        fs::write(&p, b"fixture").unwrap();
        let r = sandbox_request(&p, "/usr/bin/true", &[]).expect("required sandbox tools");
        assert!(r.environment.is_empty());
        assert!(r.arguments.contains(&OsString::from("--unshare-all")));
        assert!(!r.arguments.contains(&OsString::from("--share-net")));
        assert!(!r.arguments.contains(&OsString::from("--bind")));
    }
    #[test]
    fn real_sandbox_cannot_see_home_or_write_input() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("input");
        fs::write(&p, b"unchanged").unwrap();
        let r = sandbox_request(
            &p,
            "/usr/bin/sh",
            &[
                "-c",
                "test ! -e /home && test ! -e /etc/passwd && ! printf changed > /input && test -r /input",
            ],
        ).expect("required sandbox tools");
        let out = run(&r, &AtomicBool::new(false)).unwrap();
        assert!(out.status.success(), "sandbox probe failed");
        assert_eq!(fs::read(&p).unwrap(), b"unchanged");
    }
}

#[cfg(test)]
mod containment_tests {
    use super::*;
    #[test]
    fn admission_is_bounded_and_released_on_every_return_path() {
        let counter = AtomicUsize::new(0);
        let a = admit(&counter).unwrap();
        let b = admit(&counter).unwrap();
        assert!(matches!(admit(&counter), Err(Error::Busy)));
        drop(a);
        let c = admit(&counter).unwrap();
        drop(b);
        drop(c);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }
    #[test]
    fn public_preview_refuses_third_job_before_reading_user_file() {
        let _a = admit(&ACTIVE).unwrap();
        let _b = admit(&ACTIVE).unwrap();
        let selected = Match {
            score: 0,
            path: "/not-opened".into(),
            root: "/".into(),
            root_identity: Identity {
                device: 0,
                inode: 0,
            },
            identity: Identity {
                device: 0,
                inode: 0,
            },
        };
        assert_eq!(
            preview(&selected, &AtomicBool::new(false)),
            Err(Error::Busy)
        );
        // Content checks must remain independent even while the public quota is full.
        static ISOLATED: AtomicUsize = AtomicUsize::new(0);
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("text");
        fs::write(&path, "isolated preview").unwrap();
        let selected = Match {
            score: 0,
            path: path.clone(),
            root: temporary.path().into(),
            root_identity: identity(&fs::metadata(temporary.path()).unwrap()),
            identity: identity(&fs::metadata(path).unwrap()),
        };
        assert_eq!(
            preview_with_quota(&selected, &AtomicBool::new(false), &ISOLATED),
            Ok(Preview::Text {
                text: "isolated preview".into(),
                truncated: false
            })
        );
    }
    #[test]
    fn actual_sandbox_cannot_read_inherited_host_descriptor() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        fs::write(&input, b"fixture").unwrap();
        let source = File::open(&input).unwrap();
        let fd = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 512) };
        assert!(fd >= 512);
        let inherited = unsafe { File::from_raw_fd(fd) };
        let script = format!(
            "import os\ntry:\n os.fstat({fd})\n raise AssertionError('host descriptor leaked')\nexcept OSError: pass\nprint('descriptor-boundary-verified')"
        );
        let request = sandbox_request(&input, "/usr/bin/python3", &["-c", &script]).unwrap();
        let output = run(&request, &AtomicBool::new(false)).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"descriptor-boundary-verified\n");
        assert_eq!(
            unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
    #[test]
    fn actual_decoder_sandbox_cannot_fork_spawn_threads_raise_limits_or_gain_privileges() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        fs::write(&input, b"fixture").unwrap();
        let script = r#"import os,resource,threading,ctypes,errno
assert os.getuid()!=0
assert resource.getrlimit(resource.RLIMIT_NPROC)==(0,0)
try:
 resource.setrlimit(resource.RLIMIT_NPROC,(1,1))
 raise AssertionError('hard process limit was raised')
except (ValueError,PermissionError): pass
try:
 pid=os.fork()
 if pid==0: os._exit(0)
 os.waitpid(pid,0)
 raise AssertionError('fork succeeded')
except OSError as error: assert error.errno==errno.EAGAIN
try:
 pid=os.posix_spawn('/usr/bin/true',['true'],{})
 os.waitpid(pid,0)
 raise AssertionError('posix_spawn succeeded')
except OSError as error: assert error.errno==errno.EAGAIN
thread=threading.Thread(target=lambda:None)
try:
 thread.start();thread.join()
 raise AssertionError('thread creation succeeded')
except RuntimeError: pass
status=open('/proc/self/status').read()
assert 'NoNewPrivs:\t1' in status
assert 'CapEff:\t0000000000000000' in status
libc=ctypes.CDLL(None,use_errno=True)
assert libc.unshare(0x10000000)==-1
print('process-and-privilege-boundaries-verified')
"#;
        let request = sandbox_request(&input, "/usr/bin/python3", &["-c", script]).unwrap();
        let output = run(&request, &AtomicBool::new(false)).unwrap();
        assert!(
            output.status.success(),
            "containment probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            b"process-and-privilege-boundaries-verified\n"
        );
    }
}
