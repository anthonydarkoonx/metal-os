//! Real-time (periodic, deadline) scheduling policy.
//!
//! A real-time task is described by a *reservation*: `runtime` units of CPU time in every `period`
//! (deadline = end of the period). The scheduler runs such tasks earliest-deadline-first ahead of
//! every other class, charges each job's CPU time against its budget and throttles a task that
//! overruns, so one misbehaving task cannot take more than its reserved share from the rest of the
//! system. Admission control keeps the sum of reserved shares on each core below a ceiling; with
//! implicit deadlines and one core per task, EDF meets every deadline while the share on a core
//! stays at or below 100% (Liu & Layland, 1973), and the ceiling leaves headroom for kernel time.
//!
//! Everything here is plain arithmetic on an abstract time unit (the kernel uses its counter ticks),
//! so it can be tested on the host.

pub const PPM: u64 = 1_000_000;

/// Largest share of one core that may be reserved (parts per million). The rest is left for
/// the kernel, for interrupts and for everything that is not real-time.
pub const CORE_LIMIT_PPM: u64 = 800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub runtime: u64,
    pub period: u64,
}

impl Params {
    pub fn valid(&self) -> bool {
        self.runtime > 0 && self.runtime <= self.period && self.period < u64::MAX / PPM
    }

    /// Reserved share of a core, rounded up.
    pub fn util_ppm(&self) -> u64 {
        (self.runtime * PPM).div_ceil(self.period)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    Invalid,
    NoCapacity,
}

/// Reserved share per core.
pub struct Admission<const N: usize> {
    used: [u64; N],
}

impl<const N: usize> Admission<N> {
    pub const fn new() -> Self {
        Admission { used: [0; N] }
    }

    pub fn used(&self, core: usize) -> u64 {
        self.used[core]
    }

    /// Reserve `p` on `core`. Returns the share reserved (hand it back to `release`).
    pub fn admit_on(&mut self, core: usize, p: Params) -> Result<u64, Reject> {
        if !p.valid() {
            return Err(Reject::Invalid);
        }
        let u = p.util_ppm();
        if core >= N || self.used[core] + u > CORE_LIMIT_PPM {
            return Err(Reject::NoCapacity);
        }
        self.used[core] += u;
        Ok(u)
    }

    /// Reserve `p` on the first core of `order` that has room.
    pub fn admit(&mut self, order: &[usize], p: Params) -> Result<(usize, u64), Reject> {
        if !p.valid() {
            return Err(Reject::Invalid);
        }
        for &c in order {
            if let Ok(u) = self.admit_on(c, p) {
                return Ok((c, u));
            }
        }
        Err(Reject::NoCapacity)
    }

    pub fn release(&mut self, core: usize, share: u64) {
        self.used[core] = self.used[core].saturating_sub(share);
    }
}

/// The state of one real-time task's current job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    pub params: Params,
    /// Start of the current period. The deadline is one period later.
    pub release: u64,
    /// CPU time left in the current period.
    pub budget: u64,
    /// The job has finished and the task waits for the next release.
    pub waiting: bool,
    /// Periods that ended with the job unfinished.
    pub missed: u32,
    /// Periods started.
    pub jobs: u32,
}

impl Job {
    pub fn start(params: Params, now: u64) -> Job {
        Job { params, release: now, budget: params.runtime, waiting: false, missed: 0, jobs: 1 }
    }

    pub fn deadline(&self) -> u64 {
        self.release + self.params.period
    }

    /// When the next period starts (the instant a finished job's task is due again).
    pub fn next_release(&self) -> u64 {
        self.deadline()
    }

    /// The budget is used up and the period is not over: the task may not run until it is.
    pub fn throttled(&self, now: u64) -> bool {
        !self.waiting && self.budget == 0 && now < self.deadline()
    }

    /// Account CPU time the task just used.
    pub fn charge(&mut self, ran: u64) {
        self.budget = self.budget.saturating_sub(ran);
    }

    /// Bring the job up to `now`: start the new period if one has begun. A job still unfinished
    /// when its period ends has missed its deadline; its task gets a fresh budget regardless.
    pub fn advance(&mut self, now: u64) {
        let d = self.deadline();
        if now < d {
            return;
        }
        let boundaries = (now - d) / self.params.period + 1;
        // An unfinished job missed every deadline it ran past. A finished one only counts the
        // periods that began and ended while its task was still asleep (the task was dispatched
        // too late to run them at all); the one it is woken for on time is not a miss.
        let missed = if self.waiting { boundaries - 1 } else { boundaries };
        self.missed = self.missed.saturating_add(missed.min(u32::MAX as u64) as u32);
        self.release += boundaries * self.params.period;
        self.budget = self.params.runtime;
        self.waiting = false;
        self.jobs = self.jobs.wrapping_add(boundaries as u32);
    }

    /// The task says its job is done. Returns the time it should be woken (the next release, or
    /// `now` when it finished late and the next period is already running).
    pub fn finish(&mut self, now: u64) -> u64 {
        if now >= self.deadline() {
            self.missed = self.missed.saturating_add(1);
            let boundaries = (now - self.release) / self.params.period;
            self.release += boundaries * self.params.period;
            self.budget = self.params.runtime;
            self.waiting = false;
            self.jobs = self.jobs.wrapping_add(boundaries as u32);
            now
        } else {
            self.waiting = true;
            self.deadline()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Params = Params { runtime: 2, period: 10 };

    #[test]
    fn share_is_rounded_up_and_validated() {
        assert_eq!(P.util_ppm(), 200_000);
        assert_eq!(Params { runtime: 1, period: 3 }.util_ppm(), 333_334);
        assert!(!Params { runtime: 0, period: 10 }.valid());
        assert!(!Params { runtime: 11, period: 10 }.valid());
        assert!(!Params { runtime: 1, period: u64::MAX }.valid());
    }

    #[test]
    fn admission_stops_at_the_ceiling_per_core() {
        let mut a = Admission::<2>::new();
        let q = Params { runtime: 4, period: 10 }; // 40%
        assert_eq!(a.admit_on(0, q), Ok(400_000));
        assert_eq!(a.admit_on(0, q), Ok(400_000)); // exactly 80%
        assert_eq!(a.admit_on(0, Params { runtime: 1, period: 100 }), Err(Reject::NoCapacity));
        assert_eq!(a.admit(&[0, 1], q), Ok((1, 400_000)));
        assert_eq!(a.admit(&[0], q), Err(Reject::NoCapacity));
        assert_eq!(a.admit(&[0, 1], Params { runtime: 0, period: 1 }), Err(Reject::Invalid));
        a.release(0, 400_000);
        assert_eq!(a.used(0), 400_000);
        assert_eq!(a.admit_on(9, q), Err(Reject::NoCapacity));
    }

    #[test]
    fn a_job_that_finishes_early_sleeps_until_the_next_release() {
        let mut j = Job::start(P, 100);
        assert_eq!(j.deadline(), 110);
        j.charge(1);
        assert_eq!(j.finish(101), 110);
        assert!(j.waiting);
        j.advance(109);
        assert!(j.waiting); // not yet
        j.advance(110);
        assert!(!j.waiting);
        assert_eq!((j.release, j.budget, j.missed, j.jobs), (110, 2, 0, 2));
    }

    #[test]
    fn overrun_is_throttled_until_the_period_ends_and_counts_as_a_miss() {
        let mut j = Job::start(P, 0);
        j.charge(2);
        assert!(j.throttled(5));
        assert!(!j.throttled(10));
        j.advance(10);
        assert_eq!((j.release, j.budget, j.missed), (10, 2, 1));
        assert!(!j.throttled(10));
    }

    #[test]
    fn a_late_finish_starts_the_current_period_at_once() {
        let mut j = Job::start(P, 0);
        assert_eq!(j.finish(25), 25); // two whole periods late
        assert_eq!((j.release, j.budget, j.missed, j.waiting), (20, 2, 1, false));
        assert_eq!(j.deadline(), 30);
    }

    #[test]
    fn a_long_sleep_keeps_the_phase() {
        let mut j = Job::start(P, 0);
        j.finish(3);
        j.advance(47); // woken four periods late
        assert_eq!(j.release, 40);
        assert_eq!(j.missed, 3); // the periods that began and ended while it slept
        let mut k = Job::start(P, 0);
        k.finish(3);
        k.advance(10); // woken on time
        k.advance(15);
        assert_eq!(k.missed, 0);
    }

    #[test]
    fn running_past_the_deadline_with_budget_left_is_a_miss_too() {
        let mut j = Job::start(P, 0);
        j.charge(1);
        j.advance(10);
        assert_eq!((j.missed, j.budget, j.release), (1, 2, 10));
    }
}
