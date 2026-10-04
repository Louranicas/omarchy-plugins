//! Observation only: neither terminal state nor child reaping grants restart authority.
//! Retain an Observer before consuming Handle::finish to inspect eventual cleanup.
use acp_transport::{ReapReceipt, ReapState};
use ask_core::session::Phase;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    /// No spawn has been attempted by this backend.
    NotSpawned,
    /// Spawn was attempted but no actual cleanup receipt became available.
    Unverified,
    Child(ReapState),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub phase: Option<Phase>,
    pub terminal: bool,
    pub stop_requested: bool,
    pub thread_finished: bool,
    pub cleanup: Cleanup,
}
struct State {
    phase: Option<Phase>,
    terminal: bool,
    finished: bool,
    attempted: bool,
    receipt: Option<ReapReceipt>,
}
/// A fresh snapshot samples the retained transport receipt, including asynchronous
/// reaper completion after the backend thread exits. View snapshots may be stale.
#[derive(Clone)]
pub struct Observer {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}
impl Observer {
    pub fn snapshot(&self) -> Status {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Status {
            phase: state.phase,
            terminal: state.terminal,
            stop_requested: self.stop.load(Ordering::Acquire),
            thread_finished: state.finished,
            cleanup: state.receipt.as_ref().map_or(
                if state.attempted {
                    Cleanup::Unverified
                } else {
                    Cleanup::NotSpawned
                },
                |receipt| Cleanup::Child(receipt.state()),
            ),
        }
    }
    pub(crate) fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                phase: None,
                terminal: false,
                finished: false,
                attempted: false,
                receipt: None,
            })),
            stop,
        }
    }
    pub(crate) fn attempted(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attempted = true;
    }
    pub(crate) fn adopted(&self, receipt: ReapReceipt) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).receipt = Some(receipt);
    }
    pub(crate) fn update(&self, phase: Option<Phase>, terminal: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.phase = phase;
        state.terminal |= terminal;
    }
    pub(crate) fn guard(&self) -> Completion {
        Completion(self.clone())
    }
}
pub(crate) struct Completion(Observer);
impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.finished = true;
        state.terminal = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn actual_stolen_child_receipt_is_custody_lost_not_reaped() {
        // Only this owned child's PID is waited, never waitpid(-1). No process-wide
        // signal disposition or unrelated child is changed.
        unsafe extern "C" {
            fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        }
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("pid");
        let mut transport = acp_transport::Transport::spawn(&acp_transport::Launch {
            executable: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                "echo $$ > \"$1\"".into(),
                "owned".into(),
                record.as_os_str().to_owned(),
            ],
            directory: dir.path().to_owned(),
            environment: vec![],
        })
        .unwrap();
        let observer = Observer::new(Arc::new(AtomicBool::new(false)));
        observer.attempted();
        observer.adopted(transport.reap_receipt());
        let end = Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&record)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        };
        let mut status = 0;
        loop {
            // SAFETY: valid status pointer, positive exact owned child PID;
            // Linux WNOHANG=1 prevents this fault fixture blocking on wait.
            let result = unsafe { waitpid(pid, &mut status, 1) };
            if result == pid {
                break;
            }
            assert_eq!(result, 0);
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(transport.close(), ReapState::CustodyLost);
        drop(transport);
        assert_eq!(
            observer.snapshot().cleanup,
            Cleanup::Child(ReapState::CustodyLost)
        );
    }
}
