//! Finite process-wide custody for children that cannot be reaped inline.
use crate::Error;
use std::process::Child;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
pub const MAX_CHILDREN: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupState {
    NotSpawned,
    Running,
    Outstanding,
    Reaped,
    CustodyLost,
}
#[derive(Clone, Debug)]
pub struct CleanupReceipt(Arc<AtomicU8>);
impl CleanupReceipt {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
    pub fn state(&self) -> CleanupState {
        match self.0.load(Ordering::Acquire) {
            0 => CleanupState::NotSpawned,
            1 => CleanupState::Running,
            2 => CleanupState::Outstanding,
            3 => CleanupState::Reaped,
            _ => CleanupState::CustodyLost,
        }
    }
    pub(crate) fn set(&self, state: CleanupState) {
        self.0.store(
            match state {
                CleanupState::NotSpawned => 0,
                CleanupState::Running => 1,
                CleanupState::Outstanding => 2,
                CleanupState::Reaped => 3,
                CleanupState::CustodyLost => 4,
            },
            Ordering::Release,
        );
    }
}
pub(crate) type Retained = Box<dyn Send>;
struct Entry {
    child: Child,
    receipt: CleanupReceipt,
    _permit: Permit,
    _retained: Retained,
}
struct Shared {
    count: AtomicUsize,
    pending: Mutex<Vec<Entry>>,
    wake: Condvar,
}
pub(crate) struct Permit(Arc<Shared>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
    }
}
static OWNER: OnceLock<Result<Arc<Shared>, Error>> = OnceLock::new();
fn owner() -> Result<Arc<Shared>, Error> {
    OWNER
        .get_or_init(|| {
            let owner = Arc::new(Shared {
                count: AtomicUsize::new(0),
                pending: Mutex::new(Vec::with_capacity(MAX_CHILDREN)),
                wake: Condvar::new(),
            });
            let worker = owner.clone();
            std::thread::Builder::new()
                .name("one-shot-reaper".into())
                .spawn(move || {
                    loop {
                        let completed = {
                            let mut pending =
                                worker.pending.lock().unwrap_or_else(|e| e.into_inner());
                            while pending
                                .iter()
                                .all(|entry| entry.receipt.state() == CleanupState::CustodyLost)
                            {
                                pending =
                                    worker.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                            }
                            poll(&mut pending)
                        };
                        // User guard destructors never run under the queue lock or inside poll.
                        // Guard destructors must be bounded and panic-free; contain an accidental panic.
                        for entry in completed {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                drop(entry)
                            }));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
                .map_err(|_| Error::Spawn)?;
            Ok(owner)
        })
        .clone()
}
fn poll(pending: &mut Vec<Entry>) -> Vec<Entry> {
    let mut completed = Vec::new();
    let mut at = 0;
    while at < pending.len() {
        if pending[at].receipt.state() == CleanupState::CustodyLost {
            at += 1;
            continue;
        }
        let state = match pending[at].child.try_wait() {
            Ok(Some(_)) => Some(CleanupState::Reaped),
            Ok(None) => None,
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                pending[at].receipt.set(CleanupState::CustodyLost);
                None
            }
            Err(_) => None,
        };
        if let Some(state) = state {
            pending[at].receipt.set(state);
            completed.push(pending.swap_remove(at));
        } else {
            at += 1;
        }
    }
    completed
}
fn reserve_from(owner: Arc<Shared>) -> Result<Permit, Error> {
    owner
        .count
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_CHILDREN).then_some(n + 1)
        })
        .map_err(|_| Error::Busy)?;
    Ok(Permit(owner))
}
pub(crate) fn reserve() -> Result<Permit, Error> {
    reserve_from(owner()?)
}
pub(crate) fn transfer(child: Child, receipt: CleanupReceipt, permit: Permit, retained: Retained) {
    if receipt.state() != CleanupState::CustodyLost {
        receipt.set(CleanupState::Outstanding);
    }
    let owner = permit.0.clone();
    owner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Entry {
            child,
            receipt,
            _permit: permit,
            _retained: retained,
        });
    owner.wake.notify_one();
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            count: AtomicUsize::new(0),
            pending: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        })
    }
    #[test]
    fn reservation_is_finite_before_any_spawn() {
        let s = shared();
        let permits = (0..MAX_CHILDREN)
            .map(|_| reserve_from(s.clone()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(reserve_from(s.clone()), Err(Error::Busy)));
        drop(permits);
        assert_eq!(s.count.load(Ordering::Acquire), 0);
    }
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    #[test]
    fn blocked_entry_does_not_starve_completed_child_and_keeps_guard() {
        let s = shared();
        let counter = Arc::new(AtomicUsize::new(0));
        let slow = Command::new("/usr/bin/sleep").arg("30").spawn().unwrap();
        let slow_pid = slow.id();
        let fast = Command::new("/usr/bin/true").spawn().unwrap();
        let a = CleanupReceipt::new();
        let b = CleanupReceipt::new();
        transfer(
            slow,
            a.clone(),
            reserve_from(s.clone()).unwrap(),
            Box::new(Guard(counter.clone())),
        );
        transfer(
            fast,
            b.clone(),
            reserve_from(s.clone()).unwrap(),
            Box::new(Guard(counter.clone())),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while b.state() != CleanupState::Reaped && std::time::Instant::now() < deadline {
            let done = poll(&mut s.pending.lock().unwrap());
            drop(done);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(b.state(), CleanupState::Reaped);
        assert_eq!(a.state(), CleanupState::Outstanding);
        assert_eq!(counter.load(Ordering::Acquire), 1);
        assert_eq!(s.count.load(Ordering::Acquire), 1);
        // Test retains exclusive child custody while sending this cleanup signal.
        unsafe {
            libc::kill(slow_pid as i32, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while a.state() != CleanupState::Reaped && std::time::Instant::now() < deadline {
            drop(poll(&mut s.pending.lock().unwrap()));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(a.state(), CleanupState::Reaped);
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert_eq!(s.count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn lost_custody_quarantines_capacity_and_retained_guard() {
        let s = shared();
        let counter = Arc::new(AtomicUsize::new(0));
        let child = Command::new("/usr/bin/true").spawn().unwrap();
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child.id() as i32, &mut status, 0) },
            child.id() as i32
        ); // deliberately simulate an external reaper without Child caching
        let receipt = CleanupReceipt::new();
        transfer(
            child,
            receipt.clone(),
            reserve_from(s.clone()).unwrap(),
            Box::new(Guard(counter.clone())),
        );
        let mut queue = s.pending.lock().unwrap();
        assert!(poll(&mut queue).is_empty());
        assert_eq!(receipt.state(), CleanupState::CustodyLost);
        assert!(poll(&mut queue).is_empty());
        assert_eq!(queue.len(), 1);
        assert_eq!(s.count.load(Ordering::Acquire), 1);
        assert_eq!(counter.load(Ordering::Acquire), 0);
        // Only this test knows it already reaped the fixture before handoff.
        queue.clear();
    }
}
