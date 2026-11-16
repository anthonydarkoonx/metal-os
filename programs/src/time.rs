//! Time. `Instant` is monotonic (nanoseconds since the machine started), `SystemTime` the wall clock (from the
//! platform's real-time clock; `now()` is `None` where there is none).
pub use core::time::Duration;

const CLOCK: u64 = 34;

fn clock(op: u64) -> i64 {
    crate::svc(CLOCK, op, 0, 0, 0, 0)[0] as i64
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Instant(u64);

impl Instant {
    pub fn now() -> Instant {
        Instant(clock(0) as u64)
    }

    pub fn elapsed(&self) -> Duration {
        Instant::now().duration_since(*self)
    }

    pub fn duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    pub fn checked_add(&self, d: Duration) -> Option<Instant> {
        self.0.checked_add(u64::try_from(d.as_nanos()).ok()?).map(Instant)
    }
}

impl core::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        self.checked_add(d).expect("overflow adding a Duration to an Instant")
    }
}

impl core::ops::Sub for Instant {
    type Output = Duration;
    fn sub(self, other: Instant) -> Duration {
        self.duration_since(other)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SystemTime(u64);

/// 1970-01-01 00:00:00 UTC.
pub const UNIX_EPOCH: SystemTime = SystemTime(0);

impl SystemTime {
    pub fn now() -> Option<SystemTime> {
        match clock(1) {
            ns if ns > 0 => Some(SystemTime(ns as u64)),
            _ => None,
        }
    }

    pub fn duration_since(&self, earlier: SystemTime) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    pub fn unix_secs(&self) -> u64 {
        self.0 / 1_000_000_000
    }
}

/// Sleep for at least `d` (the system's timer ticks are 10 ms).
pub fn sleep(d: Duration) {
    let tick_ns = 10_000_000u128;
    let ticks = (d.as_nanos() + tick_ns - 1) / tick_ns;
    if ticks > 0 {
        crate::sleep(ticks as u64);
    }
}
