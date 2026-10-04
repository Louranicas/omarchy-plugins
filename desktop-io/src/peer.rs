use crate::Error;
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
};
/// A procfs observation suitable for passing from trusted compositor discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
}
pub(crate) struct Peer {
    pub identity: ProcessIdentity,
    fd: OwnedFd,
    directory: File,
    // Retain the original executable inode; do not permit recycling after unlink.
    executable: File,
}
impl Peer {
    pub fn pin(pid: u32, expected_start: Option<u64>) -> Result<Self, Error> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(Error::Invalid);
        }
        // SAFETY: pidfd_open takes a numeric PID and zero flags, returns owned fd.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful new fd is transferred exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(format!("/proc/{pid}"))?;
        let uid = directory.metadata()?.uid();
        // SAFETY: geteuid has no pointer arguments.
        if uid != unsafe { libc::geteuid() } {
            return Err(Error::Unauthenticated);
        }
        let mut bytes = Vec::new();
        File::open(format!("/proc/self/fd/{}/stat", directory.as_raw_fd()))?
            .take(8193)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(Error::Limit);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Invalid)?;
        // comm can contain spaces and ')'; final ')' closes its field.
        let end = text.rfind(')').ok_or(Error::Invalid)?;
        let start_ticks = text[end + 1..]
            .split_ascii_whitespace()
            .nth(19)
            .ok_or(Error::Invalid)?
            .parse()
            .map_err(|_| Error::Invalid)?;
        if start_ticks == 0 || expected_start.is_some_and(|expected| expected != start_ticks) {
            return Err(Error::Unauthenticated);
        }
        // Intentional kernel procfs link under a retained process directory.
        let executable = File::open(format!("/proc/self/fd/{}/exe", directory.as_raw_fd()))?;
        if !executable.metadata()?.is_file() {
            return Err(Error::Unauthenticated);
        }
        let peer = Self {
            identity: ProcessIdentity { pid, start_ticks },
            fd,
            directory,
            executable,
        };
        peer.check()?;
        Ok(peer)
    }
    pub fn check(&self) -> Result<(), Error> {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        for _ in 0..4 {
            // SAFETY: one initialized pollfd and zero timeout.
            let result = unsafe { libc::poll(&mut poll, 1, 0) };
            if result < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if result != 0 {
                return Err(Error::Unauthenticated);
            }
            let current = File::open(format!("/proc/self/fd/{}/exe", self.directory.as_raw_fd()))?
                .metadata()?;
            let original = self.executable.metadata()?;
            if !current.is_file()
                || current.dev() != original.dev()
                || current.ino() != original.ino()
                || self.directory.metadata()?.uid() != unsafe { libc::geteuid() }
            {
                return Err(Error::Unauthenticated);
            }
            // Bracket procfs observations with the same retained process lifetime.
            // A second interrupted poll refuses rather than extending the loop.
            if unsafe { libc::poll(&mut poll, 1, 0) } != 0 {
                return Err(Error::Unauthenticated);
            }
            return Ok(());
        }
        Err(Error::Unauthenticated)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_process_and_expected_start_are_checked() {
        let p = Peer::pin(std::process::id(), None).unwrap();
        p.check().unwrap();
        assert!(Peer::pin(std::process::id(), Some(p.identity.start_ticks + 1)).is_err());
    }
    #[test]
    fn exited_lifetime_cannot_authenticate() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let p = Peer::pin(child.id(), None).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(p.check(), Err(Error::Unauthenticated)));
    }
}
