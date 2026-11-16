//! Energy-aware placement: pick the core where adding a task costs the least power while still
//! leaving the task enough capacity, and decide which cores may take which work.
//!
//! The figures are *nominal and relative* (rounded numbers for a mobile big.LITTLE part of the
//! Kryo-385 class: efficiency cores about a third as fast at about an eighth of the power),
//! not measurements; the policy only needs their ratios. A platform with measured data replaces
//! `model`. Power is assumed to grow with the square of utilisation, because a core that is
//! less busy is run at a lower voltage and frequency.
use crate::{Class, CoreKind};

pub use crate::util::SCALE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Speed at the top operating point, relative to `SCALE` for the fastest core.
    pub capacity: u32,
    /// Power at full utilisation, milliwatts.
    pub active_mw: u32,
    /// Power while idle in `wfi`, milliwatts.
    pub idle_mw: u32,
    /// Extra power for a cluster that has to come out of its deepest sleep.
    pub cluster_mw: u32,
}

pub const fn model(kind: CoreKind) -> Model {
    match kind {
        CoreKind::Little => Model { capacity: 400, active_mw: 180, idle_mw: 8, cluster_mw: 15 },
        CoreKind::Big => Model { capacity: 1024, active_mw: 1500, idle_mw: 25, cluster_mw: 120 },
    }
}

/// Utilisation above which a task no longer fits a core (leaves 20% headroom).
pub const FIT: u32 = 820;

/// Power drawn by a core of `kind` at utilisation `util` (0..=SCALE).
pub fn power_mw(kind: CoreKind, util: u32) -> u32 {
    let m = model(kind);
    let u = util.min(SCALE) as u64;
    m.idle_mw + ((m.active_mw - m.idle_mw) as u64 * u * u / (SCALE as u64 * SCALE as u64)) as u32
}

/// A task's utilisation measured on a core of `kind`, as a speed-independent amount of work.
pub fn to_work(util: u32, kind: CoreKind) -> u32 {
    (util as u64 * model(kind).capacity as u64 / SCALE as u64) as u32
}

/// The same work as utilisation of a core of `kind` (may exceed `SCALE`: it does not fit).
pub fn from_work(work: u32, kind: CoreKind) -> u32 {
    (work as u64 * SCALE as u64 / model(kind).capacity as u64).min(u32::MAX as u64) as u32
}

#[derive(Clone, Copy, Debug)]
pub struct CoreState {
    pub kind: CoreKind,
    pub util: u32,
    /// Running something right now (an idle core in an idle cluster is the expensive one to wake).
    pub active: bool,
}

/// The allowed core where `work` costs the least added power. Cores it fits on win over cores
/// it does not; if it fits nowhere, the core with the most room.
pub fn place(cores: &[CoreState], work: u32, allowed: impl Fn(usize) -> bool) -> Option<usize> {
    let cluster_active = |k: CoreKind| cores.iter().any(|c| c.kind == k && c.active);
    let mut best: Option<(bool, u64, usize)> = None; // (doesn't fit, cost, index)
    for (i, c) in cores.iter().enumerate().filter(|(i, _)| allowed(*i)) {
        let after = c.util.saturating_add(from_work(work, c.kind));
        let mut cost = power_mw(c.kind, after) as i64 - power_mw(c.kind, c.util) as i64;
        if !c.active && !cluster_active(c.kind) {
            cost += model(c.kind).cluster_mw as i64;
        }
        let key = if after <= FIT { (false, cost.max(0) as u64, i) } else { (true, after as u64, i) };
        if best.map_or(true, |b| key < b) {
            best = Some(key);
        }
    }
    best.map(|b| b.2)
}

/// How long work may wait for its preferred kind of core before the other kind helps out.
#[derive(Clone, Copy, Debug)]
pub struct Spill {
    pub heavy: u64,
    pub light: u64,
}

/// May a task of `class`, with `work` of recent utilisation, run on a core of `kind` after
/// waiting `waited`? Sustained compute belongs on performance cores, everything else on efficiency
/// cores; the other kind helps when the task cannot be served well (it does not fit an efficiency
/// core, or it has waited because its own kind is saturated). Whether the cores of the preferred
/// kind exist is the caller's business (`big_online`).
pub fn may_run(class: Class, work: u32, kind: CoreKind, waited: u64, big_online: bool, spill: Spill) -> bool {
    match (class, kind) {
        (Class::Heavy, CoreKind::Big) => true,
        (Class::Heavy, CoreKind::Little) => !big_online || waited >= spill.heavy,
        (_, CoreKind::Little) => true,
        (_, CoreKind::Big) => from_work(work, CoreKind::Little) > FIT || waited >= spill.light,
    }
}

/// Utilisation a core needs to be run at to serve `util` with headroom: the request a platform
/// would pass to its frequency governor.
pub fn target_util(util: u32) -> u32 {
    (util as u64 * 5 / 4).min(SCALE as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use CoreKind::*;

    fn cs(kind: CoreKind, util: u32, active: bool) -> CoreState {
        CoreState { kind, util, active }
    }
    fn quad() -> [CoreState; 4] {
        [cs(Little, 0, false), cs(Little, 0, false), cs(Big, 0, false), cs(Big, 0, false)]
    }

    #[test]
    fn power_grows_with_utilisation_and_big_costs_more() {
        assert!(power_mw(Little, 0) < power_mw(Little, 512));
        assert!(power_mw(Little, 512) < power_mw(Little, 1024));
        assert!(power_mw(Little, 512) < power_mw(Big, 512));
        assert_eq!(power_mw(Big, 2000), power_mw(Big, 1024));
    }

    #[test]
    fn work_converts_between_core_kinds() {
        let w = to_work(512, Big);
        assert_eq!(w, 512);
        assert_eq!(from_work(w, Little), 1310); // would need 128% of a little core
        assert_eq!(to_work(1024, Little), 400);
    }

    #[test]
    fn a_light_task_goes_to_an_efficiency_core() {
        assert!(matches!(place(&quad(), 50, |_| true), Some(0 | 1)));
    }

    #[test]
    fn a_task_too_big_for_an_efficiency_core_goes_to_a_performance_core() {
        assert!(matches!(place(&quad(), 600, |_| true), Some(2 | 3)));
    }

    #[test]
    fn a_busy_efficiency_core_pushes_the_next_task_to_its_neighbour() {
        let mut c = quad();
        c[0] = cs(Little, 700, true);
        assert_eq!(place(&c, 100, |_| true), Some(1));
    }

    #[test]
    fn waking_a_sleeping_cluster_costs_extra() {
        // a sliver of work: the performance cluster is awake already, the efficiency cluster is not
        let c = [cs(Little, 0, false), cs(Big, 0, true)];
        assert_eq!(place(&c, 20, |_| true), Some(1));
        // once the efficiency cluster is awake too it wins
        let c = [cs(Little, 0, true), cs(Big, 0, true)];
        assert_eq!(place(&c, 20, |_| true), Some(0));
    }

    #[test]
    fn when_nothing_fits_the_roomiest_core_wins() {
        let c = [cs(Little, 800, true), cs(Big, 900, true)];
        assert_eq!(place(&c, 600, |_| true), Some(1)); // 900+586 vs 800+1500
        assert_eq!(place(&c, 600, |i| i == 0), Some(0));
        assert_eq!(place(&c, 600, |_| false), None);
        assert_eq!(place(&[], 1, |_| true), None);
    }

    #[test]
    fn spill_rules() {
        let s = Spill { heavy: 20, light: 5 };
        // sustained compute: performance cores always, efficiency cores only after waiting (or if there are none)
        assert!(may_run(Class::Heavy, 900, Big, 0, true, s));
        assert!(!may_run(Class::Heavy, 900, Little, 19, true, s));
        assert!(may_run(Class::Heavy, 900, Little, 20, true, s));
        assert!(may_run(Class::Heavy, 900, Little, 0, false, s));
        // light work: efficiency cores always, performance cores only if it does not fit or has waited
        assert!(may_run(Class::Interactive, 30, Little, 0, true, s));
        assert!(!may_run(Class::Interactive, 30, Big, 4, true, s));
        assert!(may_run(Class::Background, 30, Big, 5, true, s));
        assert!(may_run(Class::Interactive, 400, Big, 0, true, s)); // a whole efficiency core's worth
    }

    #[test]
    fn frequency_request_has_headroom() {
        assert_eq!(target_util(400), 500);
        assert_eq!(target_util(1000), 1024);
    }
}
