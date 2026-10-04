//! Framed presenter data transport. It authenticates an already trusted process
//! identity; app-specific messages must still carry and validate their full fence.
use crate::transport;
use desktop_io::ProcessIdentity;
use std::{io, path::Path, time::Instant};
pub struct ProcessGuard {
    process: modal_runtime::process_identity::Pinned,
}
impl ProcessGuard {
    pub fn pin(identity: ProcessIdentity) -> io::Result<Self> {
        Ok(Self {
            process: transport::pin(identity)?,
        })
    }
    pub fn check(&self) -> io::Result<()> {
        self.process.check()
    }
    pub fn check_peer(&self, uid: u32, pid: i32) -> io::Result<()> {
        self.check()?;
        if uid != unsafe { libc::geteuid() } || pid as u32 != self.process.identity().pid {
            return Err(io::Error::other("unexpected data peer"));
        }
        Ok(())
    }
}
pub struct Client {
    transport: transport::Transport,
    failed: bool,
}
impl Client {
    /// Only the configured private endpoint with the fixed modal.sock basename.
    pub fn connect(socket: &Path, expected: ProcessIdentity) -> io::Result<Self> {
        if socket.file_name().and_then(|s| s.to_str()) != Some("modal.sock") {
            return Err(io::Error::other("invalid data endpoint"));
        }
        Ok(Self {
            transport: transport::Transport::connect(
                socket
                    .parent()
                    .ok_or_else(|| io::Error::other("missing endpoint root"))?,
                expected,
            )?,
            failed: false,
        })
    }
    pub fn exchange(&mut self, frame: &[u8], deadline: Instant) -> io::Result<Vec<u8>> {
        if self.failed {
            return Err(io::Error::other("data connection failed"));
        }
        let result = self.transport.exchange(frame, deadline);
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    pub fn invalidate(&mut self) {
        self.failed = true;
        self.transport.close();
    }
}
