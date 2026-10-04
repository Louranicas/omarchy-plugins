//! Linux descriptor-pinned directory operations; advisory locks protect cooperating writers.
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub(crate) struct PrivateDir(File);
impl PrivateDir {
    pub(crate) fn open(path: &Path, private: bool) -> io::Result<Self> {
        Self::open_inner(path, private, false)
    }
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        Self::open_inner(path, true, true)
    }
    fn open_inner(path: &Path, private: bool, create: bool) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("directory must be absolute"));
        }
        let mut file = File::open("/")?;
        for component in path.components() {
            let Component::Normal(part) = component else {
                if component == Component::RootDir {
                    continue;
                }
                return Err(io::Error::other("unsafe directory component"));
            };
            let name = CString::new(part.as_bytes()).map_err(io::Error::other)?;
            // SAFETY: borrowed live directory descriptor and NUL-terminated component.
            let mut fd = unsafe {
                libc::openat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
                // SAFETY: descriptor and component remain live; mkdir never follows final symlinks.
                let result = unsafe { libc::mkdirat(file.as_raw_fd(), name.as_ptr(), 0o700) };
                if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(io::Error::last_os_error());
                }
                if result == 0 {
                    file.sync_all()?;
                }
                fd = unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: newly opened descriptor is exclusively owned.
            file = unsafe { File::from_raw_fd(fd) };
        }
        let m = file.metadata()?;
        if m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o022 != 0
            || (private && m.mode() & 0o777 != 0o700)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe directory owner/mode",
            ));
        }
        Ok(Self(file))
    }
    /// Linux procfs descriptor alias pins the parent even if its pathname is replaced.
    pub(crate) fn target(&self, original: &Path) -> io::Result<PathBuf> {
        let name = original
            .file_name()
            .ok_or_else(|| io::Error::other("missing filename"))?;
        Ok(PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name))
    }
    pub(crate) fn lock(&self, name: &str, timeout: Duration) -> io::Result<File> {
        let path = self.target(Path::new(name))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let m = file.metadata()?;
        if !m.is_file()
            || m.nlink() != 1
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe lock file",
            ));
        }
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: live descriptor; nonblocking exclusive advisory lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(file);
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "writer lock busy"));
            }
            std::thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn parent_is_pinned_and_unsafe_lock_and_modes_rejected() {
        let root = std::env::temp_dir().join(format!("agentd-private-dir-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let dir = PrivateDir::open(&root, true).unwrap();
        let moved = root.with_extension("moved");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(dir.target(Path::new("pinned")).unwrap(), b"old").unwrap();
        assert!(!root.join("pinned").exists());
        assert_eq!(fs::read(moved.join("pinned")).unwrap(), b"old");
        symlink("pinned", moved.join("lock")).unwrap();
        assert!(dir.lock("lock", Duration::ZERO).is_err());
        let _guard = dir.lock("stable", Duration::ZERO).unwrap();
        assert_eq!(
            dir.lock("stable", Duration::from_millis(10))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(PrivateDir::open(&root, false).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }
}
