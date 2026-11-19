//! Compaction *mechanism*: relocating movable pages and keeping statistics.
//! The planner is `metal-compact`; the *policy* (when and how much to compact)
//! lives in the userspace `compactd` task, which reaches this module through a
//! memory-control capability. The allocator can also call it synchronously
//! when a contiguous allocation would otherwise fail ("direct compaction").
//!
//! Locking: one compaction runs at a time (the compaction lock, outermost). For each page it
//! *plans* under the memory lock, then takes the owning task's lock and the memory lock
//! (in that order) and re-validates the plan before touching anything: between planning and
//! committing, other cores may have freed or reused the frames.
#![forbid(unsafe_code)]
use crate::mem::{self, FKind, PAGE, RAM_BASE};
use crate::sched;
use crate::sync::{SpinLock, R_COMPACT};
use crate::task::{self, State, MAX_TASKS};
use crate::mmu;
use core::sync::atomic::{AtomicU64, Ordering};
use metal_compact::Compactor;

struct Pass {
    comp: Compactor,
    /// The previous pass ran to completion; the next call starts a fresh one.
    finished: bool,
}

static PASS: SpinLock<Pass> = SpinLock::new(Pass { comp: Compactor::new(), finished: true });

static MIGRATED: AtomicU64 = AtomicU64::new(0);
static PASSES: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);
/// Contiguous allocations that failed: the demand signal for the daemon.
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// Faults on a page that was being migrated and then retried.
static SPURIOUS: AtomicU64 = AtomicU64::new(0);

fn migrate(src: usize, dst: usize, tid: usize, va: usize) -> bool {
    if tid >= task::limit() || matches!(sched::state(tid), State::Free | State::Dead) {
        return false;
    }
    let mut t = task::lock(tid);
    if t.l1 == 0 {
        return false;
    }
    let mut m = mem::lock();
    // re-validate: the world may have changed since planning
    if m.kind_at(src) != FKind::Movable || m.rmap_at(src) != (tid, va) || m.kind_at(dst) != FKind::Free {
        return false;
    }
    let (src_pa, dst_pa) = (RAM_BASE + src * PAGE, RAM_BASE + dst * PAGE);
    if !mmu::migrate_user_page(t.l1, va, src_pa, dst_pa) {
        return false;
    }
    if t.ring_pa == src_pa {
        t.ring_pa = dst_pa; // the kernel's own pointer to the task's ring page
    }
    m.move_meta(src, dst);
    true
}

/// Migrate up to `budget` pages. Returns (pages moved, pass finished).
pub fn compact(budget: usize) -> (usize, bool) {
    let mut st = PASS.lock(R_COMPACT);
    if st.finished {
        let n = mem::total_frames();
        st.comp.restart(n);
        st.finished = false;
        PASSES.fetch_add(1, Ordering::Relaxed);
    }
    let mut moved = 0;
    while moved < budget {
        let plan = {
            let m = mem::lock();
            st.comp.peek(&*m).map(|(s, d)| (s, d, m.rmap_at(s), m.kind_at(s)))
        };
        let Some((s, d, (tid, va), kind)) = plan else {
            st.finished = true;
            break;
        };
        if kind == FKind::Movable && migrate(s, d, tid, va) {
            st.comp.accept();
            moved += 1;
            // counted per page: observers also see free runs grow per page, mid-batch
            MIGRATED.fetch_add(1, Ordering::Relaxed);
        } else {
            st.comp.reject();
            REJECTED.fetch_add(1, Ordering::Relaxed);
        }
    }
    (moved, st.finished)
}

pub fn note_failure() {
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

pub fn note_spurious_fault() {
    SPURIOUS.fetch_add(1, Ordering::Relaxed);
}

pub struct Stats {
    pub migrated: u64,
    pub passes: u64,
    pub rejected: u64,
    pub failures: u64,
    pub spurious: u64,
}

pub fn stats() -> Stats {
    Stats {
        migrated: MIGRATED.load(Ordering::Relaxed),
        passes: PASSES.load(Ordering::Relaxed),
        rejected: REJECTED.load(Ordering::Relaxed),
        failures: FAILURES.load(Ordering::Relaxed),
        spurious: SPURIOUS.load(Ordering::Relaxed),
    }
}

/// Contiguous allocation. With `direct`, a failure triggers synchronous compaction
/// (in bounded batches, so the kernel lock is not held for long stretches) and a retry.
pub fn alloc_contig(n: usize, owner: u8, direct: bool) -> Option<usize> {
    if let Some(pa) = mem::alloc_contig(n, owner) {
        return Some(pa);
    }
    if direct {
        loop {
            let (moved, done) = compact(256);
            if let Some(pa) = mem::alloc_contig(n, owner) {
                return Some(pa);
            }
            if done || moved == 0 {
                break;
            }
        }
    }
    note_failure();
    None
}
