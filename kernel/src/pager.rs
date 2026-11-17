//! Writeback of swapped pages to disk, and their return, through a userspace pager (zswap's second tier).
//!
//! The swap store (swap.rs) holds compressed pages in a small RAM pool. When it fills, the memory daemon asks for
//! pages to be *written back* (`MC_SWAP_WRITEBACK`): the kernel hands cold pages, one at a time, to the **pager**, an
//! ordinary program that owns a `MemCtl` capability with `R_MEM_PAGER` and does the disk I/O through the file system. The
//! kernel never waits for a disk, and never trusts the pager with more than it asks of it:
//!
//!  * `MC_PAGER_REGISTER(slots, word)`: the caller becomes the pager. `slots` is the size of its swap file in pages;
//!    `word` the address of a 32-bit word in its memory that the kernel changes and wakes (`WAKE_WORD`) when there is work.
//!  * `MC_PAGER_NEXT(buffer)`: the next job, or none. A **write** job has the page's bytes already in `buffer` (one
//!    page-aligned page of the pager's memory) and a disk slot to put them in; a **read** job names a disk slot whose
//!    bytes the pager must put into `buffer`. One job is outstanding at a time; asking again returns the same job.
//!  * `MC_PAGER_DONE(status, buffer)`: the job is finished (status 0) or failed. Only now does the kernel commit: a
//!    written page's descriptor changes from "in the store" to "on disk (slot)" and its store entry is freed; a read page
//!    gets a frame filled from `buffer` and is mapped again.
//!
//! A task that touches a page on disk takes a translation fault; the kernel queues a read job and puts the task to sleep
//! for a tick, and it faults again until the page is back (a few retries: nothing in the scheduler had to learn a new
//! state). The few system calls that read user memory (`LOG`, `WAIT_WORD`, `WAKE_WORD`) are restarted the same way.
//!
//! What keeps this safe:
//!  * A page can be written back only if it belongs to a program (not a boot service, so the VFS server, the block
//!    driver and the pager itself can never wait for the disk) that has no threads.
//!  * A page that is touched, released or whose program dies while its write is in flight cancels the write: the commit
//!    checks that the descriptor still holds exactly the store entry that was copied.
//!  * A read whose program died, or whose page was released meanwhile, is dropped; a failed read turns the page into
//!    an unmapped one, so the program faults instead of seeing wrong data.
//!  * If the pager is gone for more than `PAGER_GRACE` ticks, programs waiting for the disk are killed.
//!
//! Limits: a thread-exit wake-up through a word on a page that is on disk is lost (such programs are not chosen while they
//! have threads, but one that starts a thread later could be waiting on an old page); the pager is a normal program, so
//! someone has to start it (the test conductor does) and restart it if it dies.
#![forbid(unsafe_code)]
use crate::mem::{self, PAGE};
use crate::mmu::{self, Page};
use crate::sched;
use crate::sync::{Global, SpinLock, R_PAGER};
use crate::syscall::{E_INVAL, E_NOMEM, E_PERM};
use crate::task::{self, State, FIRST_DYNAMIC_TASK, MAX_TASKS};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MAX_SLOTS: usize = 1024;
const QUEUE: usize = 16;
/// Ticks (1/100 s) the pager may be missing before programs waiting for it are killed.
const PAGER_GRACE: u64 = 500;

pub const JOB_NONE: u64 = 0;
pub const JOB_WRITE: u64 = 1;
pub const JOB_READ: u64 = 2;

#[derive(Clone, Copy)]
enum Op {
    /// A page copied out of the store: (task, address space, address, store handle, disk slot, cancelled).
    Write { tid: u8, l1: usize, va: usize, handle: u32, slot: u32, cancelled: bool },
    Read { tid: u8, va: usize, slot: u32 },
}

struct Pager {
    tid: Option<usize>,
    word_va: usize,
    nslots: usize,
    used: [u64; MAX_SLOTS / 64],
    want: usize,
    cur: Option<Op>,
    ins: [Option<(u8, usize, u32)>; QUEUE],
    /// Where the next victim search starts.
    cursor: (usize, usize),
    seq: u32,
    /// The tick at which the pager was first found missing (0: it is there).
    missing_since: u64,
}

static P: SpinLock<Pager> = SpinLock::new(Pager {
    tid: None,
    word_va: 0,
    nslots: 0,
    used: [0; MAX_SLOTS / 64],
    want: 0,
    cur: None,
    ins: [None; QUEUE],
    cursor: (FIRST_DYNAMIC_TASK, 0),
    seq: 0,
    missing_since: 0,
});

/// A page is (or may be) on disk: swap.rs consults the pager only then.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Somebody queued a job from a place where the pager could not be woken (a system call under a task lock).
static KICK_PENDING: AtomicBool = AtomicBool::new(false);
/// The page whose bytes a write job carries (single job at a time, single pager: no sharing).
static SCRATCH: Global<[u8; PAGE]> = Global::new([0; PAGE]);

/// Flash wear: at most this many pages are written back in any hour (32 MB); past it the pager is given no more work until the hour is over, and the
/// memory stays in the compressed store (then programs are asked to give memory back, as they are when there is no pager at all).
pub const WRITE_CAP_PER_HOUR: u64 = 8192;
const HOUR_TICKS: u64 = 360_000;
/// The tick the current hour started at, and the pages committed to disk in it.
static WINDOW_START: AtomicU64 = AtomicU64::new(0);
static WINDOW_WRITES: AtomicU64 = AtomicU64::new(0);
/// How many times a write job was held back because the hour's budget was used up.
static DEFERRED: AtomicU64 = AtomicU64::new(0);

/// Whether the hour's budget allows another page (and the hour is rolled over when it is out).
fn budget_left() -> bool {
    let now = crate::timer::ticks();
    if now.saturating_sub(WINDOW_START.load(Ordering::Relaxed)) >= HOUR_TICKS {
        WINDOW_START.store(now, Ordering::Relaxed);
        WINDOW_WRITES.store(0, Ordering::Relaxed);
    }
    WINDOW_WRITES.load(Ordering::Relaxed) < WRITE_CAP_PER_HOUR
}

static WRITTEN: AtomicU64 = AtomicU64::new(0);
static READ: AtomicU64 = AtomicU64::new(0);
static CANCELLED: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);

fn alive(tid: usize) -> bool {
    !matches!(sched::state(tid), State::Free | State::Dead)
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn alloc_slot(p: &mut Pager) -> Option<u32> {
    for s in 0..p.nslots {
        if p.used[s / 64] >> (s % 64) & 1 == 0 {
            p.used[s / 64] |= 1 << (s % 64);
            return Some(s as u32);
        }
    }
    None
}

pub fn free_slot(slot: u32) {
    let mut p = P.lock(R_PAGER);
    if (slot as usize) < MAX_SLOTS {
        p.used[slot as usize / 64] &= !(1 << (slot % 64));
    }
}

// ---- the pager's side ------------------------------------------------------------------------------------------------

pub fn register(cur: usize, nslots: usize, word_va: usize) -> i64 {
    let me = task::aspace(cur);
    if nslots == 0 || nslots > MAX_SLOTS || word_va % 4 != 0 || task::members(me).count() != 1 {
        return E_INVAL;
    }
    let mut p = P.lock(R_PAGER);
    if let Some(old) = p.tid {
        if old != me && alive(old) {
            return crate::syscall::E_FULL; // somebody else is the pager
        }
    }
    if p.tid.is_some() && nslots < p.nslots && p.used.iter().any(|&w| w != 0) {
        return E_INVAL; // pages are on disk: the file cannot shrink
    }
    p.tid = Some(me);
    p.word_va = word_va;
    p.nslots = nslots.max(p.nslots);
    p.missing_since = 0;
    ACTIVE.store(true, Ordering::Relaxed);
    0
}

fn is_pager(me: usize) -> bool {
    P.lock(R_PAGER).tid == Some(me)
}

/// May the page of this program be written to disk?
fn eligible(tid: usize, pager: Option<usize>) -> bool {
    tid >= FIRST_DYNAMIC_TASK
        && task::aspace(tid) == tid
        && Some(tid) != pager
        && !matches!(sched::state(tid), State::Free | State::Dead | State::Embryo)
        && task::members(tid).count() == 1
}

/// `MC_PAGER_NEXT(buffer)`: (job kind, slot), with the bytes of a write job in the caller's buffer.
pub fn next(cur: usize, buf_va: usize) -> Result<(u64, u64), i64> {
    let me = task::aspace(cur);
    if buf_va % PAGE != 0 || !is_pager(me) {
        return Err(if buf_va % PAGE != 0 { E_INVAL } else { E_PERM });
    }
    // 1. a job already out, or a page waiting to come back
    let op = {
        let mut p = P.lock(R_PAGER);
        if p.cur.is_none() {
            if let Some(i) = p.ins.iter().position(|q| q.is_some()) {
                let (tid, va, slot) = p.ins[i].take().unwrap();
                p.cur = Some(Op::Read { tid, va, slot });
            }
        }
        p.cur
    };
    let op = match op {
        Some(op) => Some(op),
        None => start_write(me),
    };
    match op {
        None => Ok((JOB_NONE, 0)),
        Some(Op::Read { slot, .. }) => Ok((JOB_READ, slot as u64)),
        Some(Op::Write { slot, .. }) => {
            let t = task::lock(me);
            let Some(pa) = crate::swap::translate_writable(&t, me, buf_va) else { return Err(E_INVAL) };
            if !crate::frame::phys::try_copy_to(pa, &SCRATCH.get()[..PAGE]) {
                return Err(E_INVAL); // not RAM (a device page)
            }
            Ok((JOB_WRITE, slot as u64))
        }
    }
}

/// Pick a page in the store, copy it to `SCRATCH` and make it the current job.
fn start_write(me: usize) -> Option<Op> {
    let (want, (mut tid, mut from)) = {
        let p = P.lock(R_PAGER);
        (p.want, p.cursor)
    };
    if want == 0 {
        return None;
    }
    if !budget_left() {
        DEFERRED.fetch_add(1, Ordering::Relaxed);
        return None; // (the wish stays: it is served when the hour is over)
    }
    let mut wrapped = false;
    let mut scanned = 0;
    while scanned <= MAX_TASKS {
        if tid >= MAX_TASKS {
            if wrapped {
                break;
            }
            wrapped = true;
            tid = FIRST_DYNAMIC_TASK;
            from = 0;
        }
        scanned += 1;
        if eligible(tid, Some(me)) {
            let t = task::lock(tid);
            if t.l1 != 0 {
                if let Some((va, handle)) = mmu::find_swapped(t.l1, from) {
                    let mut p = P.lock(R_PAGER);
                    let Some(slot) = alloc_slot(&mut p) else {
                        p.want = 0; // the swap file is full
                        return None;
                    };
                    if !crate::swap::load_to(handle, SCRATCH.get()) {
                        p.used[slot as usize / 64] &= !(1 << (slot % 64));
                        return None;
                    }
                    let op = Op::Write { tid: tid as u8, l1: t.l1, va, handle, slot, cancelled: false };
                    p.cur = Some(op);
                    p.want -= 1;
                    p.cursor = (tid, va + PAGE);
                    return Some(op);
                }
            }
        }
        tid += 1;
        from = 0;
    }
    P.lock(R_PAGER).want = 0; // nothing left that may be written
    None
}

/// `MC_PAGER_DONE(status, buffer)`.
pub fn done(cur: usize, status: u64, buf_va: usize) -> i64 {
    let me = task::aspace(cur);
    if !is_pager(me) {
        return E_PERM;
    }
    let Some(op) = P.lock(R_PAGER).cur else { return E_INVAL };
    match op {
        Op::Write { tid, l1, va, handle, slot, .. } => {
            let mut committed = false;
            if status == 0 {
                // Everything that voids a write (a touch, a release, the program's end) happens under this task lock, so the
                // flag read under it is final: the commit cannot land after the program's pages were given back.
                let t = task::lock(tid as usize);
                let cancelled = matches!(P.lock(R_PAGER).cur, Some(Op::Write { cancelled: true, .. }));
                if !cancelled && t.l1 == l1 && l1 != 0 && mmu::swapped_to_disk(l1, va, handle, slot) {
                    crate::swap::discard_handle(handle);
                    committed = true;
                }
            }
            if committed {
                WRITTEN.fetch_add(1, Ordering::Relaxed);
                WINDOW_WRITES.fetch_add(1, Ordering::Relaxed);
            } else {
                free_slot(slot);
                if status != 0 {
                    FAILED.fetch_add(1, Ordering::Relaxed);
                } else {
                    CANCELLED.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Op::Read { tid, va, slot } => {
            let tid = tid as usize;
            if status != 0 {
                FAILED.fetch_add(1, Ordering::Relaxed);
                // the bytes are lost: the page becomes unmapped, and the program faults when it next touches it
                let t = task::lock(tid);
                if t.l1 != 0 && matches!(mmu::inspect(t.l1, va), Page::Disk(s) if s == slot) {
                    mmu::clear_swapped(t.l1, va);
                    free_slot(slot);
                }
            } else {
                let owner = mem::OWNER_TASK0 + tid as u8;
                let Some(pa) = mem::alloc_movable(owner, tid, va) else { return E_NOMEM }; // (the job stays: ask again)
                let copied = {
                    let t = task::lock(me);
                    match crate::swap::translate(&t, me, buf_va) {
                        Some(src) if buf_va % PAGE == 0 => crate::frame::phys::try_copy_page(pa, src),
                        _ => false,
                    }
                };
                if !copied {
                    mem::free(pa);
                    return E_INVAL;
                }
                let t = task::lock(tid);
                if t.l1 != 0 && alive(tid) && matches!(mmu::inspect(t.l1, va), Page::Disk(s) if s == slot) && mmu::install_resident(t.l1, va, pa) {
                    free_slot(slot);
                    READ.fetch_add(1, Ordering::Relaxed);
                } else {
                    // The program is gone or let go of the page (whoever did that gave the slot back), or it is dying and
                    // will: either way the frame is not needed.
                    mem::free(pa);
                }
            }
        }
    }
    P.lock(R_PAGER).cur = None;
    kick_if_work();
    0
}

/// The memory daemon wants `n` more pages written back.
pub fn want(n: usize) -> i64 {
    let mut p = P.lock(R_PAGER);
    if p.tid.is_none() {
        return E_PERM;
    }
    p.want = (p.want + n).min(1 << 16);
    drop(p);
    kick();
    0
}

// ---- the kernel's side ---------------------------------------------------------------------------------------------

pub enum Wait {
    /// A read job is queued (or running): sleep and retry.
    Sleep,
    /// No pager, and none within the grace period: the page is lost.
    Lost,
}

/// A task of program `tid` needs the page at `va` that is on disk in `slot`. Called with the task's lock held.
pub fn request_in(tid: usize, va: usize, slot: u32) -> Wait {
    let now = crate::timer::ticks();
    let mut p = P.lock(R_PAGER);
    match p.tid {
        Some(pg) if alive(pg) => p.missing_since = 0,
        _ => {
            if p.missing_since == 0 {
                p.missing_since = now.max(1);
            }
            if now.saturating_sub(p.missing_since) > PAGER_GRACE {
                return Wait::Lost;
            }
        }
    }
    let same = |t: u8, v: usize| t as usize == tid && v == va;
    let known = matches!(p.cur, Some(Op::Read { tid: t, va: v, .. }) if same(t, v)) || p.ins.iter().flatten().any(|&(t, v, _)| same(t, v));
    if !known {
        if let Some(i) = p.ins.iter().position(|q| q.is_none()) {
            p.ins[i] = Some((tid as u8, va, slot));
        }
    }
    KICK_PENDING.store(true, Ordering::Relaxed);
    Wait::Sleep
}

/// The page at `va` of the space `l1` is being brought back, released or its program is gone: a write job for it is void.
pub fn cancel_write(l1: usize, va: Option<usize>) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut p = P.lock(R_PAGER);
    if let Some(Op::Write { l1: l, va: v, cancelled, .. }) = &mut p.cur {
        if *l == l1 && va.map_or(true, |x| x == *v) {
            *cancelled = true;
        }
    }
}

/// Wake the pager (nothing may be locked). Does nothing if there is no work or no pager.
pub fn kick() {
    KICK_PENDING.store(false, Ordering::Relaxed);
    let (tid, va, seq) = {
        let mut p = P.lock(R_PAGER);
        let Some(tid) = p.tid else { return };
        p.seq = p.seq.wrapping_add(1);
        (tid, p.word_va, p.seq)
    };
    if alive(tid) {
        let t = task::lock(tid);
        crate::thread::store_and_wake(&t, tid, va, seq);
    }
}

pub fn kick_if_pending() {
    if KICK_PENDING.load(Ordering::Relaxed) {
        kick();
    }
}

fn kick_if_work() {
    let more = {
        let p = P.lock(R_PAGER);
        p.want > 0 || p.ins.iter().any(|q| q.is_some())
    };
    if more {
        kick();
    }
}

pub struct Stats {
    pub written: u64,
    pub read: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub slots_used: u64,
    pub registered: bool,
    /// Pages committed to disk in the current hour, and write jobs held back by the hourly cap.
    pub hour_writes: u64,
    pub deferred: u64,
}

pub fn stats() -> Stats {
    let p = P.lock(R_PAGER);
    Stats {
        written: WRITTEN.load(Ordering::Relaxed),
        read: READ.load(Ordering::Relaxed),
        cancelled: CANCELLED.load(Ordering::Relaxed),
        failed: FAILED.load(Ordering::Relaxed),
        slots_used: p.used.iter().map(|w| w.count_ones() as u64).sum(),
        registered: p.tid.map_or(false, alive),
        hour_writes: WINDOW_WRITES.load(Ordering::Relaxed),
        deferred: DEFERRED.load(Ordering::Relaxed),
    }
}
