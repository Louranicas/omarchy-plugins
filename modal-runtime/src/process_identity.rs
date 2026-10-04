//! Pinned process lifetime and executable inode for controlled host admission.
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
};
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub pid: u32,
    pub start_ticks: u64,
    pub executable_device: u64,
    pub executable_inode: u64,
}
pub struct Pinned {
    identity: Identity,
    pidfd: OwnedFd,
    directory: File,
}
impl Pinned {
    pub fn capture(pid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::Error::other("invalid pid"));
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY)
            .open(format!("/proc/{pid}"))?;
        if directory.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("foreign process"));
        }
        let identity = inspect(pid, &directory)?;
        let result = Self {
            identity,
            pidfd,
            directory,
        };
        result.check()?;
        Ok(result)
    }
    pub fn pin(expected: Identity) -> io::Result<Self> {
        let process = Self::capture(expected.pid)?;
        if process.identity != expected {
            return Err(io::Error::other("process identity mismatch"));
        }
        Ok(process)
    }
    pub fn identity(&self) -> Identity {
        self.identity
    }
    pub fn check(&self) -> io::Result<()> {
        for _ in 0..4 {
            let mut poll = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut poll, 1, 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n != 0 || inspect(self.identity.pid, &self.directory)? != self.identity {
                return Err(io::Error::other("process lifetime or executable changed"));
            }
            // A second poll brackets the procfs observations.
            if unsafe { libc::poll(&mut poll, 1, 0) } != 0 {
                return Err(io::Error::other("process changed during check"));
            }
            return Ok(());
        }
        Err(io::Error::other("process check interrupted"))
    }
}
fn inspect(pid: u32, directory: &File) -> io::Result<Identity> {
    let base = format!("/proc/self/fd/{}", directory.as_raw_fd());
    let mut bytes = Vec::new();
    File::open(format!("{base}/stat"))?
        .take(8193)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(io::Error::other("proc stat too large"));
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let end = text
        .rfind(')')
        .ok_or_else(|| io::Error::other("invalid proc stat"))?;
    let start_ticks = text[end + 1..]
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::other("missing start"))?
        .parse::<u64>()
        .map_err(io::Error::other)?;
    if start_ticks == 0 {
        return Err(io::Error::other("invalid start"));
    }
    // Intentional kernel procfs executable link: inspect its target inode, not a caller path.
    let executable = File::open(format!("{base}/exe"))?.metadata()?;
    if !executable.is_file() {
        return Err(io::Error::other("invalid executable"));
    }
    Ok(Identity {
        pid,
        start_ticks,
        executable_device: executable.dev(),
        executable_inode: executable.ino(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expected_identity_and_exited_child_are_refused() {
        let current = Pinned::capture(std::process::id()).unwrap();
        current.check().unwrap();
        let mut wrong = current.identity();
        wrong.start_ticks += 1;
        assert!(Pinned::pin(wrong).is_err());
        wrong = current.identity();
        wrong.executable_inode += 1;
        assert!(Pinned::pin(wrong).is_err());
        let mut child = std::process::Command::new("/usr/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pinned = Pinned::capture(child.id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(pinned.check().is_err());
    }
}
