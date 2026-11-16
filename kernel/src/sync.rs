//! Kernel locking.
//!
//! `SpinLock<T>` is a plain test-and-set spinlock with an RAII guard. Every
//! acquisition names a *rank*; a core may only acquire a lock whose rank is
//! strictly greater than every lock it already holds. Since all code orders
//! its acquisitions this way no cycle of waiting cores can form, and the check
//! runs on every acquisition so a violation panics immediately instead of
//! deadlocking once in a million runs.
//!
//! The ranks, outermost first:
//!   COMPACT < EP(e) < TASK(t) < RING(t) < NODES < IRQ < OBJ < SMMU < SWAP < MEM < CONSOLE
//! (the scheduler is lock-free; see sched.rs)
//! (within a family, lower index first). Kernel code runs with interrupts
//! masked, so a lock holder is never interrupted by code wanting the same lock.
use crate::smp::MAX_CPUS;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Serialises SPAWN (taken outside every task lock).
pub const R_SPAWNING: u16 = 5;
/// The table of threads waiting on words (thread.rs); taken under the address space's task lock, above every task lock.
pub const R_WORDS: u16 = 266;
pub const R_COMPACT: u16 = 10;
pub const R_EP: u16 = 100;
pub const R_TASK: u16 = 200;
pub const R_RING: u16 = 270;
/// Real-time admission (per-core reserved shares). Taken when a task dies, i.e. under the task locks.
pub const R_RT: u16 = 336;
pub const R_NODES: u16 = 340;
pub const R_IRQ: u16 = 345;
pub const R_OBJ: u16 = 350;
pub const R_SMMU: u16 = 355;
/// The swap store (compressed pages). Taken under a task lock; never together with MEM.
/// The pager's bookkeeping (pager.rs): taken under a task lock, and above it the swap store may be taken.
pub const R_PAGER: u16 = 357;
pub const R_SWAP: u16 = 358;
/// The layout-randomisation generator (rand.rs); drawn from under task locks.
pub const R_RAND: u16 = 359;
pub const R_MEM: u16 = 360;
pub const R_CONSOLE: u16 = 380;

const MAX_HELD: usize = 128; // every endpoint and every task lock at once (World::lock), with room to spare

struct Held {
    depth: usize,
    rank: [u16; MAX_HELD],
}

struct PerCpu(UnsafeCell<Held>);
unsafe impl Sync for PerCpu {}

static HELD: [PerCpu; MAX_CPUS] = [const { PerCpu(UnsafeCell::new(Held { depth: 0, rank: [0; MAX_HELD] })) }; MAX_CPUS];

fn held() -> &'static mut Held {
    // Only the owning core touches its own entry, with interrupts masked.
    unsafe { &mut *HELD[crate::smp::cpu_id()].0.get() }
}

pub const CLASS_NAMES: [&str; 10] = ["compact", "endpoint", "task", "ring", "nodes", "irq", "objects", "memory", "scheduler", "console"];

fn class_of(rank: u16) -> usize {
    match rank {
        0..=99 => 0,
        100..=199 => 1,
        200..=269 => 2,
        270..=339 => 3,
        340..=344 => 4,
        345..=349 => 5,
        350..=359 => 6,
        360..=369 => 7,
        _ => 9,
    }
}

/// Lock statistics, one block per core: counting on a shared cache line would make every lock
/// operation on every core bounce it around, which is exactly the cost the locks were split to avoid.
struct Counts {
    acquires: [AtomicU64; 10],
    waits: [AtomicU64; 10],
}

#[repr(align(128))]
struct CoreCounts(Counts);

static COUNTS: [CoreCounts; MAX_CPUS] = [const {
    CoreCounts(Counts { acquires: [const { AtomicU64::new(0) }; 10], waits: [const { AtomicU64::new(0) }; 10] })
}; MAX_CPUS];

/// Only the owning core writes its block, so a plain load + store is enough (no read-modify-write).
fn bump(counter: &AtomicU64) {
    counter.store(counter.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
}

/// (acquisitions, acquisitions that had to wait) per lock class, summed over all cores.
pub fn stats(class: usize) -> (u64, u64) {
    COUNTS.iter().fold((0, 0), |(a, w), c| (a + c.0.acquires[class].load(Ordering::Relaxed), w + c.0.waits[class].load(Ordering::Relaxed)))
}

#[cold]
fn order_violation(want: u16, top: u16) -> ! {
    crate::uart::panic_print(format_args!("[kernel] PANIC: lock order violation: acquiring rank {want} while holding rank {top}"));
    crate::sched::semihost_exit(4)
}

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    rank: u16,
}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    /// Acquire at `rank` (must exceed the rank of every lock this core already holds).
    pub fn lock(&self, rank: u16) -> Guard<'_, T> {
        let h = held();
        if h.depth > 0 && h.rank[h.depth - 1] >= rank {
            order_violation(rank, h.rank[h.depth - 1]);
        }
        if h.depth == MAX_HELD {
            order_violation(rank, 0);
        }
        let c = class_of(rank);
        let counts = &COUNTS[crate::smp::cpu_id()].0;
        bump(&counts.acquires[c]);
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            bump(&counts.waits[c]);
            while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                unsafe { asm!("wfe", options(nomem, nostack)) };
            }
        }
        h.rank[h.depth] = rank;
        h.depth += 1;
        Guard { lock: self, rank }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        let h = held();
        // usually the innermost lock, but die()/revoke release a whole set in one go
        if let Some(i) = (0..h.depth).rev().find(|&i| h.rank[i] == self.rank) {
            h.rank.copy_within(i + 1..h.depth, i);
            h.depth -= 1;
        }
        self.lock.locked.store(false, Ordering::Release);
        unsafe { asm!("sev", options(nomem, nostack)) };
    }
}

/// Globals that are only written during single-core boot, or that belong to one core.
/// Not a lock: callers uphold the access rule stated at each use.
pub struct Global<T>(UnsafeCell<T>);

unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(v: T) -> Self {
        Global(UnsafeCell::new(v))
    }

    /// Callers must not hold the returned reference across a call that can
    /// obtain another one to the same global.
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}
