//! Custody of one dedicated presenter child, never an Ask provider/session supervisor.
use omarchy_process_runner::{CleanupReceipt, CleanupState, ReapReservation};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Presenter {
    child: Option<Child>,
    pid: u32,
    reaped: bool,
    reservation: Option<ReapReservation>,
    receipt: Option<CleanupReceipt>,
    #[cfg(test)]
    skip_signal: bool,
}
impl Presenter {
    /// Execute the retained validated ELF inode. CLOEXEC closes the descriptor
    /// only after the kernel has resolved this procfs executable path. Scripts
    /// are refused because interpreter execution requires a different FD contract.
    pub fn spawn_pinned(
        program: &File,
        args: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let metadata = program.metadata()?;
        let mut magic = [0; 4];
        if !metadata.is_file()
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
            || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
            || program.read_at(&mut magic, 0)? != 4
            || magic != *b"\x7fELF"
        {
            return Err(io::Error::other(
                "pinned presenter must be trusted ELF executable",
            ));
        }
        Self::spawn(
            Path::new(&format!("/proc/self/fd/{}", program.as_raw_fd())),
            args,
            environment,
        )
    }
    /// Executable/arguments/environment come from trusted installation configuration,
    /// never socket clients. No shell, inherited environment or output capture.
    pub fn spawn(
        program: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let mut bytes = 0usize;
        let mut count = 0;
        for part in std::iter::once(program.as_os_str())
            .chain(args.iter().map(OsString::as_os_str))
            .chain(
                environment
                    .iter()
                    .flat_map(|(k, v)| [k.as_os_str(), v.as_os_str()]),
            )
        {
            bytes = bytes
                .checked_add(part.len())
                .ok_or_else(|| io::Error::other("presenter arguments"))?;
            count += 1;
            if bytes > 65536 || count > 256 || part.as_bytes().contains(&0) {
                return Err(io::Error::other("presenter arguments"));
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        if !program.is_absolute()
            || environment.iter().any(|(key, _)| {
                key.is_empty() || key.as_bytes().contains(&b'=') || !keys.insert(key)
            })
        {
            return Err(io::Error::other("presenter configuration"));
        }
        let reservation = ReapReservation::reserve()
            .map_err(|_| io::Error::other("presenter reap capacity unavailable"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(environment.iter().cloned())
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // SAFETY: async-signal-safe syscall only; no inherited host descriptors.
        unsafe {
            command.pre_exec(|| {
                if libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) < 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn()?;
        Ok(Self {
            pid: child.id(),
            child: Some(child),
            reaped: false,
            reservation: Some(reservation),
            receipt: None,
            #[cfg(test)]
            skip_signal: false,
        })
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn cleanup_receipt(&self) -> Option<CleanupReceipt> {
        self.receipt.clone()
    }
    fn handoff(&mut self, lost: bool) {
        if let Some(child) = self.child.take() {
            let reservation = self
                .reservation
                .take()
                .expect("reserved before presenter spawn");
            self.receipt = Some(if lost {
                reservation.quarantine(child, ())
            } else {
                reservation.handoff(child, ())
            });
        }
    }
    pub fn alive(&mut self) -> io::Result<bool> {
        if self.reaped || self.child.is_none() {
            return Ok(false);
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.handoff(true);
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } == 0)
    }
    fn kill_group(&self) {
        #[cfg(test)]
        if self.skip_signal {
            return;
        }
        if !self.reaped && self.child.is_some() {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
    }
    fn try_reap(&mut self) -> io::Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                self.reaped = true;
                self.reservation = None;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => {
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.handoff(true);
                }
                Err(error)
            }
        }
    }
    pub fn revoke(&mut self, deadline: Instant) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        if let Some(receipt) = &self.receipt {
            return match receipt.state() {
                CleanupState::Reaped => {
                    self.reaped = true;
                    Ok(())
                }
                CleanupState::CustodyLost => Err(io::Error::other("presenter custody lost")),
                _ => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "presenter cleanup outstanding",
                )),
            };
        }
        loop {
            // WNOWAIT checks custody before any numeric process-group signal.
            match self.alive() {
                Ok(_) => {
                    self.kill_group();
                    if self.try_reap()? {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                self.handoff(false);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "presenter cleanup outstanding",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}
impl Drop for Presenter {
    fn drop(&mut self) {
        if self.reaped || self.child.is_none() {
            return;
        }
        if self.alive().is_ok() {
            self.kill_group();
            let _ = self.try_reap();
        }
        if !self.reaped && self.child.is_some() {
            self.handoff(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_elf_descriptor_survives_path_replacement_and_rejects_scripts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("presenter");
        std::fs::copy("/usr/bin/sleep", &path).unwrap();
        let pinned = File::open(&path).unwrap();
        let expected = pinned.metadata().unwrap();
        std::fs::rename(&path, root.path().join("original")).unwrap();
        std::fs::copy("/usr/bin/false", &path).unwrap();
        let mut presenter = Presenter::spawn_pinned(&pinned, &["30".into()], &[]).unwrap();
        let actual = std::fs::metadata(format!("/proc/{}/exe", presenter.pid())).unwrap();
        assert_eq!(
            (actual.dev(), actual.ino()),
            (expected.dev(), expected.ino())
        );
        presenter
            .revoke(Instant::now() + Duration::from_secs(1))
            .unwrap();
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(Presenter::spawn_pinned(&File::open(path).unwrap(), &[], &[]).is_err());
    }
    #[test]
    fn dedicated_stopped_presenter_can_be_revoked() {
        let mut child = Presenter::spawn(Path::new("/usr/bin/sleep"), &["30".into()], &[]).unwrap();
        assert_eq!(unsafe { libc::kill(child.pid() as i32, libc::SIGSTOP) }, 0);
        child
            .revoke(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(child.reaped);
    }
    #[test]
    fn invalid_presenter_configuration_never_spawns() {
        assert!(Presenter::spawn(Path::new("relative"), &[], &[]).is_err());
        assert!(
            Presenter::spawn(
                Path::new("/bin/true"),
                &[],
                &[("X".into(), "a".into()), ("X".into(), "b".into())]
            )
            .is_err()
        );
    }
    #[test]
    fn timeout_transfers_presenter_without_blocking_and_reap_is_separate_proof() {
        let mut child = Presenter::spawn(Path::new("/usr/bin/sleep"), &["30".into()], &[]).unwrap();
        child.skip_signal = true;
        let pid = child.pid();
        let result = child.revoke(Instant::now());
        let receipt = child.cleanup_receipt().unwrap();
        let before = receipt.state();
        let start = Instant::now();
        drop(child);
        let elapsed = start.elapsed();
        // The controlled child is still owned by the shared reaper until exit.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while receipt.state() == CleanupState::Outstanding && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(result.is_err());
        assert_eq!(before, CleanupState::Outstanding);
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(receipt.state(), CleanupState::Reaped);
    }
}
