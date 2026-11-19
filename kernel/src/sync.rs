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
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// Serialises SPAWN (taken outside every task lock).
pub const R_SPAWNING: u16 = 5;
/// Serialises the lifecycle operations (task.rs `lifecycle`, `World`): outside SPAWN's lock and every endpoint and task lock.
pub const R_TEARDOWN: u16 = 3;
/// The table of threads waiting on words (thread.rs); taken under the address space's task lock, above every task lock.
pub const R_WORDS: u16 = 266;
pub const R_COMPACT: u16 = 10;
pub const R_EP: u16 = 100;
pub const R_TASK: u16 = 200;
pub const R_RING: u16 = 270;
/// Claiming a run-time endpoint (ipc.rs `claim_dynamic`: the quota is counted and the claim made under it). Under a task lock.
pub const R_EPCLAIM: u16 = 330;
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
// SAFETY: each entry is used only by the core whose id indexes it (see `held`), so no entry is shared between cores.
unsafe impl Sync for PerCpu {}

static HELD: [PerCpu; MAX_CPUS] = [const { PerCpu(UnsafeCell::new(Held { depth: 0, rank: [0; MAX_HELD] })) }; MAX_CPUS];

fn held() -> &'static mut Held {
    // Only the owning core touches its own entry, with interrupts masked.
    // SAFETY: entry `cpu_id()` belongs to this core alone and interrupts are masked in the kernel, so no handler can hold a second `&mut` to it;
    // callers use the reference briefly and never across another `held()` call.
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

// SAFETY: the `locked` flag gives exclusive access to the `UnsafeCell`, so sharing a lock between cores is fine when `T: Send`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: the lock only owns a `T`; moving it to another core moves a `T: Send`.
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
                // SAFETY: WFE only pauses until the holder's SEV (or any event); it changes no state the compiler tracks.
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
        // SAFETY: a `Guard` exists only while `locked` is set by this guard, so no other reference to the data exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the only reference through the guard.
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
        // SAFETY: SEV only wakes cores waiting in WFE; it changes no state the compiler tracks.
        unsafe { asm!("sev", options(nomem, nostack)) };
    }
}

// ---- borrow-checked globals -----------------------------------------------------------------------------------------------
//
// Some kernel state has no lock because of an ownership rule ("only the core running the task writes its frame", "only boot
// touches the tables"). Rust cannot see such rules, so these cells check them at run time instead: every access takes a
// guard, and an access that overlaps another one it may not overlap panics on the spot. A broken rule becomes a deterministic
// kernel panic, never two live `&mut` to the same memory.

const WRITER: i32 = -1;

fn borrow_violation(what: &str) -> ! {
    crate::uart::panic_print(format_args!("[kernel] PANIC: overlapping borrow of {what}"));
    crate::sched::semihost_exit(4)
}

/// Shared access to one value (see `Slots::read`).
pub struct Ref<'a, T> {
    state: &'a AtomicI32,
    data: &'a T,
}

/// Exclusive access to one value (see `Global::get`, `Slots::write`).
pub struct RefMut<'a, T> {
    state: &'a AtomicI32,
    data: *mut T,
    _p: core::marker::PhantomData<&'a mut T>,
}

/// How long a table entry's borrow waits for another core to let go before it is called an overlap. A holder on another core
/// lets go within microseconds; the same core can never get here while its own guard is alive, so it spends the whole budget
/// and then panics.
const SPINS: u32 = 5_000_000;

fn acquire_write<'a, T>(state: &'a AtomicI32, data: *mut T, what: &str, spins: u32) -> RefMut<'a, T> {
    let mut left = spins;
    while state.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_err() {
        if left == 0 {
            borrow_violation(what);
        }
        left -= 1;
        core::hint::spin_loop();
    }
    RefMut { state, data, _p: core::marker::PhantomData }
}

fn acquire_read<'a, T>(state: &'a AtomicI32, data: *const T, what: &str, spins: u32) -> Ref<'a, T> {
    let mut left = spins;
    loop {
        let s = state.load(Ordering::Relaxed);
        if s >= 0 && state.compare_exchange(s, s + 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            // SAFETY: this reader is registered in `state`, so no `&mut` to the value exists; `data` points at a live value.
            return Ref { state, data: unsafe { &*data } };
        }
        if left == 0 {
            borrow_violation(what);
        }
        left -= 1;
        core::hint::spin_loop();
    }
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.data
    }
}
impl<T> Drop for Ref<'_, T> {
    fn drop(&mut self) {
        self.state.fetch_sub(1, Ordering::Release);
    }
}
impl<T> Deref for RefMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard holds the cell's exclusive state, so nothing else refers to the value.
        unsafe { &*self.data }
    }
}
impl<T> DerefMut for RefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above.
        unsafe { &mut *self.data }
    }
}
impl<T> Drop for RefMut<'_, T> {
    fn drop(&mut self) {
        self.state.store(0, Ordering::Release);
    }
}

/// One value with no lock, for state only boot or one core touches. `get` is exclusive and checked.
pub struct Global<T> {
    state: AtomicI32,
    taken: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: every access goes through the state word (exclusive guard) or `take` (once, then no guards are handed out).
unsafe impl<T: Send> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(v: T) -> Self {
        Global { state: AtomicI32::new(0), taken: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    /// Exclusive access for as long as the guard lives. Panics if another guard on this global is alive (on this core or
    /// another), or if `take` already gave the value away.
    pub fn get(&self) -> RefMut<'_, T> {
        if self.taken.load(Ordering::Acquire) {
            borrow_violation("a global that was taken for good");
        }
        acquire_write(&self.state, self.data.get(), "a global", 0)
    }

    /// Plain, unchecked access for the one place that runs before the MMU is on (`mmu::init`).
    ///
    /// With the MMU off every data access is Device memory, and atomic read-modify-write (what the checked `get` does) on Device
    /// memory faults on real hardware (the RK3566 gives a data abort, DFSC 0x35) although QEMU allows it.
    ///
    /// # Safety
    /// Only the boot core runs (no other core is started yet), and no other reference to the value exists or is made while the
    /// returned one lives.
    #[allow(clippy::mut_from_ref)] // exclusivity is the caller's contract (see Safety)
    pub unsafe fn boot_get(&self) -> &mut T {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.data.get() }
    }

    /// Hand the value out for the rest of the kernel's life, once (an arena a long-lived allocator keeps). `None` the second
    /// time or while a guard from `get` is alive.
    #[allow(clippy::mut_from_ref)] // exclusivity is enforced at run time: `state` stays WRITER and `taken` flips once
    pub fn take(&'static self) -> Option<&'static mut T> {
        if self.state.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return None;
        }
        if self.taken.swap(true, Ordering::AcqRel) {
            return None; // (state stays WRITER, so `get` keeps refusing too)
        }
        // SAFETY: `state` is left at WRITER for good and `taken` flips once, so this is the only reference there will ever be.
        Some(unsafe { &mut *self.data.get() })
    }

    /// The value's address (to hand to hardware or assembly; using it is the caller's business).
    pub fn addr(&self) -> *mut T {
        self.data.get()
    }
}

/// `N` values, each checked on its own: the per-task and per-core tables, where different cores use different entries at the
/// same time but one entry has one user (or many readers) at a time.
pub struct Slots<T, const N: usize> {
    state: [AtomicI32; N],
    data: UnsafeCell<[T; N]>,
}

// SAFETY: an entry is reached only through its state word (many readers or one writer).
unsafe impl<T: Send, const N: usize> Sync for Slots<T, N> {}

impl<T, const N: usize> Slots<T, N> {
    pub const fn new(v: [T; N]) -> Self {
        Slots { state: [const { AtomicI32::new(0) }; N], data: UnsafeCell::new(v) }
    }

    /// Exclusive access to entry `i` (panics if it is borrowed in any way, or `i` is out of range).
    pub fn write(&self, i: usize) -> RefMut<'_, T> {
        let st = &self.state[i];
        // SAFETY: `i` is in range (indexed above); the pointer stays inside the array.
        let p = unsafe { (self.data.get() as *mut T).add(i) };
        acquire_write(st, p, "a table entry", SPINS)
    }

    /// Shared access to entry `i` (panics if it is being written).
    pub fn read(&self, i: usize) -> Ref<'_, T> {
        let st = &self.state[i];
        // SAFETY: `i` is in range (indexed above). The pointer is only turned into a reference after `acquire_read` has
        // registered this reader (no writer can be active then).
        let p = unsafe { (self.data.get() as *const T).add(i) };
        acquire_read(st, p, "a table entry", SPINS)
    }

    /// Exclusive access to the whole table (boot-time set-up; panics if any entry is borrowed).
    pub fn all_mut(&self) -> AllMut<'_, T, N> {
        for (k, st) in self.state.iter().enumerate() {
            if st.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_err() {
                for u in &self.state[..k] {
                    u.store(0, Ordering::Release);
                }
                borrow_violation("a table");
            }
        }
        AllMut { t: self }
    }
}

/// One element of a `SlotTable`.
struct SlotCell<T>(UnsafeCell<T>);
// SAFETY: a cell is reached only through its state word in `SlotTable` (many readers or one writer).
unsafe impl<T: Send> Sync for SlotCell<T> {}

/// `Slots` for a table whose length is chosen at boot (see `table.rs`): each entry checked on its own.
pub struct SlotTable<T> {
    state: crate::table::Table<AtomicI32>,
    data: crate::table::Table<SlotCell<T>>,
}

impl<T: Send> SlotTable<T> {
    pub const fn new() -> Self {
        SlotTable { state: crate::table::Table::new(), data: crate::table::Table::new() }
    }

    pub fn init(&self, n: usize, mut f: impl FnMut(usize) -> T) {
        self.state.init(n, |_| AtomicI32::new(0));
        self.data.init(n, |i| SlotCell(UnsafeCell::new(f(i))));
    }

    /// Exclusive access to entry `i` (panics if it is borrowed in any way, or `i` is out of range).
    pub fn write(&self, i: usize) -> RefMut<'_, T> {
        acquire_write(&self.state[i], self.data[i].0.get(), "a table entry", SPINS)
    }

    /// Shared access to entry `i` (panics if it is being written).
    pub fn read(&self, i: usize) -> Ref<'_, T> {
        // SAFETY (of the pointer use): `acquire_read` registers this reader before the pointer becomes a reference.
        acquire_read(&self.state[i], self.data[i].0.get(), "a table entry", SPINS)
    }

    /// Exclusive access to every entry in turn (boot-time set-up; panics if any entry is borrowed).
    pub fn for_each_mut(&self, mut f: impl FnMut(usize, &mut T)) {
        for i in 0..self.state.len() {
            f(i, &mut *self.write(i));
        }
    }
}

pub struct AllMut<'a, T, const N: usize> {
    t: &'a Slots<T, N>,
}

impl<T, const N: usize> Deref for AllMut<'_, T, N> {
    type Target = [T; N];
    fn deref(&self) -> &[T; N] {
        // SAFETY: every entry's state is WRITER, held by this guard.
        unsafe { &*self.t.data.get() }
    }
}
impl<T, const N: usize> DerefMut for AllMut<'_, T, N> {
    fn deref_mut(&mut self) -> &mut [T; N] {
        // SAFETY: as above.
        unsafe { &mut *self.t.data.get() }
    }
}
impl<T, const N: usize> Drop for AllMut<'_, T, N> {
    fn drop(&mut self) {
        for st in &self.t.state {
            st.store(0, Ordering::Release);
        }
    }
}
