//! Fail closed when the BOOTTIME–MONOTONIC offset no longer matches.
//! Sandwich sampling bounds scheduling jitter instead of guessing a fixed tolerance.
use std::io;

#[derive(Clone, Copy, Debug)]
struct Sample {
    before: i128,
    boot: i128,
    after: i128,
}
impl Sample {
    fn interval(self) -> io::Result<(i128, i128)> {
        if self.before < 0 || self.boot < self.before || self.after < self.before {
            return Err(io::Error::other("invalid suspend clock sample"));
        }
        // An unusually slow sample cannot establish continuity. Reject rather
        // than let a long scheduling pause hide a suspend inside its interval.
        if self.after - self.before > 1_000_000 {
            return Err(io::Error::other("suspend clock sample exceeds 1ms"));
        }
        Ok((self.boot - self.after, self.boot - self.before))
    }
}
fn nanos(clock: libc::clockid_t) -> io::Result<i128> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Valid stack storage; clock_gettime does not retain this pointer.
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(io::Error::other("invalid kernel clock"));
    }
    Ok(i128::from(value.tv_sec) * 1_000_000_000 + i128::from(value.tv_nsec))
}
fn sample() -> io::Result<Sample> {
    Ok(Sample {
        before: nanos(libc::CLOCK_MONOTONIC)?,
        boot: nanos(libc::CLOCK_BOOTTIME)?,
        after: nanos(libc::CLOCK_MONOTONIC)?,
    })
}
#[derive(Debug)]
pub(crate) struct SuspendGuard {
    interval: Option<(i128, i128)>,
}
impl SuspendGuard {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            interval: Some(sample()?.interval()?),
        })
    }
    pub(crate) fn check(&mut self) -> io::Result<()> {
        self.observe(sample())
    }
    fn observe(&mut self, sample: io::Result<Sample>) -> io::Result<()> {
        let previous = self
            .interval
            .take()
            .ok_or_else(|| io::Error::other("clock continuity lost"))?;
        let next = sample?.interval()?;
        let intersection = (previous.0.max(next.0), previous.1.min(next.1));
        if intersection.0 > intersection.1 {
            return Err(io::Error::other("suspend or clock discontinuity detected"));
        }
        self.interval = Some(intersection);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn invalidate_for_test(&mut self) {
        self.interval = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn reading(before: i128, delta: i128, width: i128) -> io::Result<Sample> {
        Ok(Sample {
            before,
            boot: before + delta + width / 2,
            after: before + width,
        })
    }
    #[test]
    fn jitter_intervals_overlap_but_suspend_is_sticky() {
        let mut guard = SuspendGuard {
            interval: Some((95, 105)),
        };
        guard.observe(reading(1000, 100, 8)).unwrap();
        guard.observe(reading(2000, 100, 2)).unwrap();
        assert!(guard.observe(reading(3000, 110, 2)).is_err());
        assert!(guard.observe(reading(4000, 100, 2)).is_err());
    }
    #[test]
    fn ambiguous_long_sample_and_read_failure_poison() {
        for next in [
            reading(1000, 100, 1_000_001),
            Err(io::Error::other("clock failure")),
            Ok(Sample {
                before: 10,
                boot: 20,
                after: 9,
            }),
        ] {
            let mut guard = SuspendGuard {
                interval: Some((0, 200)),
            };
            assert!(guard.observe(next).is_err());
            assert!(guard.observe(reading(2000, 100, 2)).is_err());
        }
    }
    #[test]
    fn actual_linux_clock_is_sampled() {
        let mut guard = SuspendGuard::new().unwrap();
        for _ in 0..100 {
            guard.check().unwrap();
        }
    }
}
