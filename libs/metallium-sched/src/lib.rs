//! Scheduling policy as pure logic, unit-tested on the host: asymmetric (big.LITTLE / DynamIQ)
//! core placement here, real-time reservations in `rt`, utilisation tracking in `util`,
//! energy-aware placement in `energy`, time slices below. The kernel feeds them its topology and
//! measurements.
#![forbid(unsafe_code)]
#![no_std]

pub mod energy;
pub mod rt;
pub mod util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreKind {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Housekeeping and async I/O completions: stay on efficient cores.
    Background,
    /// Latency-sensitive but light: efficient cores until they saturate.
    Interactive,
    /// Sustained compute: performance cores.
    Heavy,
}

#[derive(Debug, Clone, Copy)]
pub struct Core {
    pub kind: CoreKind,
    /// 0..=100 utilisation estimate.
    pub load: u32,
    /// Thermal budget exhausted: avoid unless nothing else is available.
    pub throttled: bool,
}

/// Length of a time slice, microseconds: how long a task of this class may run before an equal
/// competitor gets the core. Latency-sensitive work gets short slices (it rarely uses them up);
/// sustained compute gets long ones, which means fewer context switches and warmer caches.
pub const fn quantum_us(c: Class) -> u64 {
    match c {
        Class::Interactive => 2_000,
        Class::Background => 5_000,
        Class::Heavy => 15_000,
    }
}

/// Where a task stands in the run order: a level (0 real-time, then 1 interactive, 2 background,
/// 3 heavy; lower runs first) and, at the real-time level, the deadline that orders its tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rank {
    pub level: u8,
    pub deadline: u64,
}

impl Rank {
    /// The rank of a task that serves nobody and has no reservation, at `level`.
    pub const fn of_level(level: u8) -> Rank {
        Rank { level, deadline: u64::MAX }
    }

    /// What a server inherits from the caller it is serving: whichever of the two is more urgent.
    /// (A level-0 caller hands its deadline on, so the server is ordered among the real-time tasks by it.)
    pub fn inherit(self, caller: Rank) -> Rank {
        Rank { level: self.level.min(caller.level), deadline: self.deadline.min(caller.deadline) }
    }

    /// Must `self` run before `other`? Strictly: a lower level, or at the real-time level an earlier deadline.
    pub fn outranks(self, other: Rank) -> bool {
        self.level < other.level || (self.level == 0 && other.level == 0 && self.deadline < other.deadline)
    }
}

/// Utilisation above which an Interactive task is promoted to a big core.
pub const INTERACTIVE_PROMOTE_LOAD: u32 = 70;

fn least_loaded(cores: &[Core], f: impl Fn(&Core) -> bool) -> Option<usize> {
    cores
        .iter()
        .enumerate()
        .filter(|(_, c)| f(c))
        .min_by_key(|(i, c)| (c.load, *i))
        .map(|(i, _)| i)
}

pub fn place(cores: &[Core], class: Class) -> Option<usize> {
    let ok = |c: &Core| !c.throttled;
    let of = |k: CoreKind| move |c: &Core| !c.throttled && c.kind == k;
    let (first, second) = match class {
        Class::Heavy => (CoreKind::Big, CoreKind::Little),
        Class::Background => (CoreKind::Little, CoreKind::Big),
        Class::Interactive => {
            let little = least_loaded(cores, of(CoreKind::Little));
            if let Some(i) = little {
                if cores[i].load < INTERACTIVE_PROMOTE_LOAD {
                    return Some(i);
                }
            }
            (CoreKind::Big, CoreKind::Little)
        }
    };
    least_loaded(cores, of(first))
        .or_else(|| least_loaded(cores, of(second)))
        .or_else(|| least_loaded(cores, ok))
        .or_else(|| least_loaded(cores, |_| true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use CoreKind::*;

    fn c(kind: CoreKind, load: u32, throttled: bool) -> Core {
        Core { kind, load, throttled }
    }
    const L: CoreKind = Little;
    const B: CoreKind = Big;

    #[test]
    fn rank_orders_levels_and_real_time_deadlines() {
        let rt = |d| Rank { level: 0, deadline: d };
        let lvl = Rank::of_level;
        assert!(rt(500).outranks(lvl(1)));
        assert!(!lvl(1).outranks(rt(500)));
        assert!(rt(100).outranks(rt(200)));
        assert!(!rt(200).outranks(rt(100)));
        assert!(!rt(100).outranks(rt(100)));
        assert!(lvl(1).outranks(lvl(2)) && lvl(2).outranks(lvl(3)));
        assert!(!lvl(2).outranks(lvl(2))); // equal levels take turns, they do not preempt
    }

    #[test]
    fn a_server_inherits_the_urgency_of_the_caller_it_serves() {
        let server = Rank::of_level(2); // a background server ...
        let rt_caller = Rank { level: 0, deadline: 1_000 };
        let served = server.inherit(rt_caller);
        assert_eq!(served, Rank { level: 0, deadline: 1_000 });
        // ... now outranks an interactive hog, and a real-time task with a later deadline
        assert!(served.outranks(Rank::of_level(1)));
        assert!(served.outranks(Rank { level: 0, deadline: 2_000 }));
        assert!(!served.outranks(Rank { level: 0, deadline: 500 }));
        // inheriting from a less urgent caller changes nothing
        assert_eq!(Rank::of_level(1).inherit(Rank::of_level(3)), Rank::of_level(1));
        // transitive: the server's own server inherits the same
        assert_eq!(Rank::of_level(3).inherit(served), served);
    }

    #[test]
    fn slices_grow_with_how_cpu_bound_the_class_is() {
        assert!(quantum_us(Class::Interactive) < quantum_us(Class::Background));
        assert!(quantum_us(Class::Background) < quantum_us(Class::Heavy));
    }

    #[test]
    fn classes_prefer_their_cluster() {
        let cores = [c(L, 10, false), c(L, 5, false), c(B, 0, false), c(B, 20, false)];
        assert_eq!(place(&cores, Class::Heavy), Some(2));
        assert_eq!(place(&cores, Class::Background), Some(1));
        assert_eq!(place(&cores, Class::Interactive), Some(1));
    }

    #[test]
    fn interactive_spills_to_big_when_little_saturated() {
        let cores = [c(L, 90, false), c(L, 80, false), c(B, 30, false), c(B, 10, false)];
        assert_eq!(place(&cores, Class::Interactive), Some(3));
    }

    #[test]
    fn thermal_throttle_is_avoided_but_not_fatal() {
        let cores = [c(L, 0, false), c(B, 0, true)];
        assert_eq!(place(&cores, Class::Heavy), Some(0));
        let all_hot = [c(L, 5, true), c(B, 1, true)];
        assert_eq!(place(&all_hot, Class::Heavy), Some(1));
        assert_eq!(place(&[], Class::Heavy), None);
    }
}
