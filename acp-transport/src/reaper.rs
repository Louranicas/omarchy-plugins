//! One bounded process-wide owner for children whose reap cannot complete inline.
//! It never signals children: group signalling must occur before custody transfer.
use crate::Error;
use std::process::Child;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const MAX_CHILDREN: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapState {
    Running,
    Outstanding,
    Reaped,
    CustodyLost,
}
#[derive(Clone)]
pub struct Receipt(Arc<AtomicU8>);
impl Receipt {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
    pub fn state(&self) -> ReapState {
        match self.0.load(Ordering::Acquire) {
            0 => ReapState::Running,
            1 => ReapState::Outstanding,
            2 => ReapState::Reaped,
            _ => ReapState::CustodyLost,
        }
    }
    pub(crate) fn outstanding(&self) {
        self.0.store(1, Ordering::Release);
    }
    pub(crate) fn lost(&self) {
        self.0.store(3, Ordering::Release);
    }
}
struct Entry {
    child: Child,
    receipt: Receipt,
    _permit: Permit,
}
struct Shared {
    count: AtomicUsize,
    pending: Mutex<Vec<Entry>>,
    quarantined: Mutex<Vec<Entry>>,
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
                quarantined: Mutex::new(Vec::with_capacity(MAX_CHILDREN)),
            });
            let worker = owner.clone();
            std::thread::Builder::new()
                .name("acp-child-reaper".into())
                .spawn(move || {
                    loop {
                        {
                            let mut pending =
                                worker.pending.lock().unwrap_or_else(|e| e.into_inner());
                            let mut i = 0;
                            while i < pending.len() {
                                let done = match pending[i].child.try_wait() {
                                    Ok(Some(_)) => {
                                        pending[i].receipt.0.store(2, Ordering::Release);
                                        true
                                    }
                                    Ok(None) => false,
                                    Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                                        pending[i].receipt.lost();
                                        true
                                    }
                                    Err(_) => false,
                                };
                                if done {
                                    let entry = pending.swap_remove(i);
                                    if entry.receipt.state() == ReapState::CustodyLost {
                                        worker
                                            .quarantined
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .push(entry);
                                    }
                                } else {
                                    i += 1;
                                }
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
                .map_err(|_| Error::Spawn)?;
            Ok(owner)
        })
        .clone()
}
pub(crate) fn reserve() -> Result<Permit, Error> {
    reserve_from(owner()?)
}
fn reserve_from(owner: Arc<Shared>) -> Result<Permit, Error> {
    owner
        .count
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_CHILDREN).then_some(n + 1)
        })
        .map_err(|_| Error::Limit)?;
    Ok(Permit(owner))
}
pub(crate) fn transfer(child: Child, receipt: Receipt, permit: Permit) {
    receipt.outstanding();
    let owner = permit.0.clone();
    owner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Entry {
            child,
            receipt,
            _permit: permit,
        });
}
pub(crate) fn quarantine(child: Child, receipt: Receipt, permit: Permit) {
    receipt.lost();
    let owner = permit.0.clone();
    owner
        .quarantined
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Entry {
            child,
            receipt,
            _permit: permit,
        });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_bounded_before_any_fork() {
        // Isolated owner avoids racing real process tests against global capacity.
        let owner = Arc::new(Shared {
            count: AtomicUsize::new(0),
            pending: Mutex::new(vec![]),
            quarantined: Mutex::new(vec![]),
        });
        let mut permits = Vec::new();
        for _ in 0..MAX_CHILDREN {
            permits.push(reserve_from(owner.clone()).unwrap());
        }
        assert!(matches!(reserve_from(owner.clone()), Err(Error::Limit)));
        permits.pop();
        assert!(reserve_from(owner.clone()).is_ok());
        drop(permits);
        assert_eq!(owner.count.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod quarantine_tests {
    use super::*;
    #[test]
    fn custody_loss_quarantines_admission_without_claiming_reap() {
        let owner = Arc::new(Shared {
            count: AtomicUsize::new(0),
            pending: Mutex::new(vec![]),
            quarantined: Mutex::new(vec![]),
        });
        let permit = reserve_from(owner.clone()).unwrap();
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let receipt = Receipt::new();
        quarantine(child, receipt.clone(), permit);
        assert_eq!(receipt.state(), ReapState::CustodyLost);
        assert_eq!(owner.count.load(Ordering::Acquire), 1);
        assert_eq!(owner.quarantined.lock().unwrap().len(), 1);
        // Test-only teardown breaks retained ownership after separately proven exit.
        owner.quarantined.lock().unwrap().clear();
        assert_eq!(owner.count.load(Ordering::Acquire), 0);
    }
}
