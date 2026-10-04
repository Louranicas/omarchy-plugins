//! Nonmodal source recovery. This module never owns a Client, lease or presenter.
use crate::{Error, Runtime, View, native::Native};
use desktop_io::Endpoint;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Policy defaults, not measured latency guarantees. All attempts share one
/// absolute transport deadline; filesystem and scheduler stalls are not cancellable.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub initial: Duration,
    pub maximum: Duration,
    pub jitter: Duration,
    pub attempt: Duration,
    pub failures: u8,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            maximum: Duration::from_secs(2),
            jitter: Duration::from_millis(50),
            attempt: Duration::from_millis(500),
            failures: 8,
        }
    }
}
impl Policy {
    fn validate(self) -> Result<Self, Error> {
        if self.initial < Duration::from_millis(1)
            || self.initial > self.maximum
            || self.maximum > Duration::from_secs(30)
            || self.jitter > self.maximum
            || self.attempt < Duration::from_millis(1)
            || self.attempt > Duration::from_secs(1)
            || self.failures == 0
            || self.failures > 32
        {
            return Err(Error::Invalid);
        }
        Ok(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Waiting { failures: u8, retry_in: Duration },
    Ready,
    Exhausted,
}
struct Schedule {
    policy: Policy,
    failures: u8,
    due: Duration,
    last: Duration,
    exhausted: bool,
}
impl Schedule {
    fn new(policy: Policy) -> Result<Self, Error> {
        Ok(Self {
            policy: policy.validate()?,
            failures: 0,
            due: Duration::ZERO,
            last: Duration::ZERO,
            exhausted: false,
        })
    }
    fn check(&mut self, now: Duration) -> Result<bool, Error> {
        if now < self.last {
            self.exhausted = true;
            return Err(Error::Stale);
        }
        self.last = now;
        Ok(!self.exhausted && now >= self.due)
    }
    fn failed(&mut self, now: Duration, random: u64) -> Result<(), Error> {
        self.failures = self.failures.checked_add(1).ok_or(Error::Exhausted)?;
        if self.failures >= self.policy.failures {
            self.exhausted = true;
            return Ok(());
        }
        let base = self
            .policy
            .initial
            .saturating_mul(1u32 << (self.failures - 1))
            .min(self.policy.maximum);
        // Millisecond jitter with inclusive upper bound; constructor limits cast.
        let jitter = Duration::from_millis(random % (self.policy.jitter.as_millis() as u64 + 1));
        self.due = now
            .checked_add(base)
            .and_then(|n| n.checked_add(jitter))
            .ok_or(Error::Exhausted)?;
        Ok(())
    }
    fn status(&self, now: Duration) -> Status {
        if self.exhausted {
            Status::Exhausted
        } else {
            Status::Waiting {
                failures: self.failures,
                retry_in: self.due.saturating_sub(now),
            }
        }
    }
}
/// Retains the exact discovered endpoint/process identity. It cannot discover a
/// replacement compositor, acquire authority or replay an activation after loss.
pub struct Sources {
    endpoint: Arc<Endpoint>,
    native: Option<Native>,
    runtime: Runtime,
    schedule: Schedule,
    origin: Instant,
    previous_epoch: Option<[u8; 16]>,
}
impl Sources {
    pub(crate) fn resume(
        native: Native,
        mut runtime: Runtime,
        origin: Instant,
        policy: Policy,
    ) -> Result<Self, Error> {
        let schedule = Schedule::new(policy)?;
        runtime.pending = None;
        runtime.effects.clear();
        runtime.selection.close(runtime.selection.generation())?;
        Ok(Self {
            endpoint: native.endpoint(),
            native: Some(native),
            runtime,
            schedule,
            origin,
            previous_epoch: None,
        })
    }

    /// `origin` must be the original clock origin used for the supplied Runtime.
    pub fn new(
        endpoint: Endpoint,
        mut runtime: Runtime,
        origin: Instant,
        policy: Policy,
    ) -> Result<Self, Error> {
        let schedule = Schedule::new(policy)?;
        offline(&mut runtime)?;
        Ok(Self {
            endpoint: Arc::new(endpoint),
            native: None,
            runtime,
            schedule,
            origin,
            previous_epoch: None,
        })
    }
    /// Metadata may remain visible as stale during an outage; never actionable.
    pub fn view(&self) -> View {
        self.runtime.view()
    }
    pub fn status(&self) -> Status {
        if self.schedule.exhausted {
            Status::Exhausted
        } else if self.native.is_some() {
            Status::Ready
        } else {
            self.schedule.status(self.origin.elapsed())
        }
    }
    /// At most one reconnect attempt, or one source event, per call. No sleep.
    pub fn step(&mut self) -> Result<Status, Error> {
        let result = self.step_with(Instant::now(), entropy);
        if result.is_err() {
            self.native = None;
            self.schedule.exhausted = true;
            offline(&mut self.runtime)?;
        }
        result
    }
    fn step_with(
        &mut self,
        now: Instant,
        mut random: impl FnMut() -> Result<[u8; 24], Error>,
    ) -> Result<Status, Error> {
        let elapsed = now
            .checked_duration_since(self.origin)
            .ok_or(Error::Stale)?;
        let due = self.schedule.check(elapsed)?;
        if self.schedule.exhausted {
            return Ok(Status::Exhausted);
        }
        let ms = elapsed
            .as_millis()
            .try_into()
            .map_err(|_| Error::Exhausted)?;
        let deadline = now
            .checked_add(self.schedule.policy.attempt)
            .ok_or(Error::Exhausted)?;
        if let Some(native) = &mut self.native {
            if native.poll_before(&mut self.runtime, ms, deadline).is_ok() {
                return Ok(Status::Ready);
            }
            self.native = None;
            offline(&mut self.runtime)?;
            // A failure never reconnects in the same step or replays its event.
            let bytes = random().inspect_err(|_| self.schedule.exhausted = true)?;
            self.schedule.failed(
                self.origin.elapsed(),
                u64::from_le_bytes(bytes[16..].try_into().unwrap()),
            )?;
            return Ok(self.status());
        }
        if !due {
            return Ok(self.status());
        }
        let bytes = random().inspect_err(|_| self.schedule.exhausted = true)?;
        let epoch: [u8; 16] = bytes[..16].try_into().unwrap();
        if epoch == [0; 16] || self.previous_epoch == Some(epoch) {
            self.schedule.exhausted = true;
            return Err(Error::Exhausted);
        }
        self.previous_epoch = Some(epoch);
        let attempt = Native::connect_before(self.endpoint.clone(), epoch, deadline)
            .map_err(|_| Error::Stale)
            .and_then(|mut native| {
                native.synchronize_before(&mut self.runtime, ms, deadline)?;
                if Instant::now() >= deadline {
                    return Err(Error::Stale);
                }
                Ok(native)
            });
        match attempt {
            Ok(native) => {
                self.native = Some(native);
                self.schedule.failures = 0;
                Ok(Status::Ready)
            }
            Err(_) => {
                offline(&mut self.runtime)?;
                self.schedule.failed(
                    self.origin.elapsed(),
                    u64::from_le_bytes(bytes[16..].try_into().unwrap()),
                )?;
                Ok(self.status())
            }
        }
    }
    /// Explicit ownership transfer after another freshness check. The caller must
    /// independently create a new Client/acquire/presenter/target view; this object
    /// has no modal token to transfer. Failed transfer drops the closed source.
    pub fn take_ready(mut self) -> Result<(Native, Runtime, Instant), Error> {
        if self.step()? != Status::Ready {
            return Err(Error::Stale);
        }
        // Drain a bounded backlog before exposing a snapshot to a new authority.
        let end = Instant::now() + self.schedule.policy.attempt;
        for _ in 0..32 {
            let ms = self
                .origin
                .elapsed()
                .as_millis()
                .try_into()
                .map_err(|_| Error::Exhausted)?;
            if !self
                .native
                .as_mut()
                .ok_or(Error::Stale)?
                .poll_before(&mut self.runtime, ms, end)?
            {
                if Instant::now() >= end {
                    offline(&mut self.runtime)?;
                    return Err(Error::Stale);
                }
                return Ok((
                    self.native.take().ok_or(Error::Stale)?,
                    self.runtime,
                    self.origin,
                ));
            }
        }
        Err(Error::Stale)
    }
}
fn offline(runtime: &mut Runtime) -> Result<(), Error> {
    runtime.pending = None;
    runtime.effects.clear();
    runtime.selection.close(runtime.selection.generation())?;
    runtime.attention.disconnected()?;
    Ok(())
}
fn entropy() -> Result<[u8; 24], Error> {
    let mut bytes = [0; 24];
    let read =
        unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), libc::GRND_NONBLOCK) };
    if read != bytes.len() as isize {
        return Err(Error::Exhausted);
    }
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_backoff_jitter_cap_and_finite_exhaustion() {
        let mut s = Schedule::new(Policy::default()).unwrap();
        let mut time = Duration::ZERO;
        assert!(s.check(time).unwrap());
        for failure in 1..8 {
            s.failed(time, 50).unwrap();
            let delay = Duration::from_millis((100u64 << (failure - 1)).min(2000) + 50);
            assert_eq!(s.due, time + delay);
            assert!(!s.check(s.due - Duration::from_nanos(1)).unwrap());
            time = s.due;
            assert!(s.check(time).unwrap());
        }
        s.failed(time, 0).unwrap();
        assert_eq!(s.status(time), Status::Exhausted);
        assert!(!s.check(time + Duration::from_secs(100)).unwrap());
    }
    #[test]
    fn policy_and_clock_refuse_unbounded_or_backward_work() {
        for p in [
            Policy {
                failures: 0,
                ..Policy::default()
            },
            Policy {
                failures: 33,
                ..Policy::default()
            },
            Policy {
                initial: Duration::ZERO,
                ..Policy::default()
            },
            Policy {
                attempt: Duration::from_secs(2),
                ..Policy::default()
            },
            Policy {
                jitter: Duration::from_secs(3),
                ..Policy::default()
            },
        ] {
            assert!(Schedule::new(p).is_err());
        }
        let mut s = Schedule::new(Policy::default()).unwrap();
        s.check(Duration::from_secs(2)).unwrap();
        assert!(s.check(Duration::from_secs(1)).is_err());
        assert!(s.exhausted);
    }
    #[test]
    fn outage_clears_selection_and_pending_activation_before_retry() {
        let mut r = Runtime::fixture().unwrap();
        r.execute(crate::Command::Open, 0).unwrap();
        let view = r.view();
        r.execute(
            crate::Command::Activate {
                generation: view.generation,
                revision: view.revision,
                id: view.rows[0].id,
            },
            0,
        )
        .unwrap();
        assert!(r.pending.is_some());
        offline(&mut r).unwrap();
        assert!(r.pending.is_none());
        assert!(!r.view().open);
        assert!(r.view().stale);
        assert!(r.effects.is_empty());
    }
}
