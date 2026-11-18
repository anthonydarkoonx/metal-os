//! Waiting on words: the one primitive threads build locks, condition variables and joins from.
//!
//! `WAIT_WORD(address, expected, timeout)` blocks the caller if the 32-bit word at `address` (in its program's address
//! space) still holds `expected`, until a `WAKE_WORD` on the same address by a task of the same program, or until
//! `timeout` ticks have passed (0 = no limit). The check and the going to sleep are one step with respect to
//! `WAKE_WORD`: a waker that changes the word *and then* wakes can never be missed.
//!
//!  * returns 0 when woken, `E_AGAIN` if the word had another value, `E_TIMEDOUT` when the time ran out.
//!  * `WAKE_WORD(address, count)` wakes up to `count` waiters, longest-waiting first; returns how many.
//!  * `THREAD_EXIT(address, value)` stores `value` in the word at `address` and wakes everybody waiting on it, then
//!    ends the thread. That is how a thread is joined: its stack can be reused as soon as the word changes, because the
//!    kernel does the store only when the thread has finished with the stack.
//!
//! Words in a program's own (movable) memory are per program: other programs cannot wait for or wake them. A word in
//! memory that is shared by capability — a shared memory object, a DMA buffer: anything that never moves — is identified by
//! its physical address instead, so the programs that have the page mapped can wake each other (that is how the network
//! service wakes a program that is waiting for its data: the page both have mapped holds the word).
#![forbid(unsafe_code)]
use crate::sched;
use crate::sync::{SpinLock, R_WORDS};
use crate::task::{self, frame, lock, State, MAX_TASKS};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const E_AGAIN: i64 = -10;
pub const E_TIMEDOUT: i64 = crate::syscall::E_TIMEDOUT;
pub const E_INVAL: i64 = crate::syscall::E_INVAL;

static TABLE: SpinLock<()> = SpinLock::new(());
/// Address each waiting task waits on, and its place in the queue.
static KEY: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];
static TICKET: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

pub enum Wait {
    Ret(i64),
    Blocked,
}

pub fn wait(cur: usize, va: usize, expected: u32, timeout_ticks: u64) -> Wait {
    if va % 4 != 0 {
        return Wait::Ret(E_INVAL);
    }
    let space = task::aspace(cur);
    let t = lock(space); // the frame cannot move while we hold this
    let Some(pa) = crate::swap::translate(&t, space, va) else { return Wait::Ret(E_INVAL) };
    if !is_ram_word(pa) {
        return Wait::Ret(E_INVAL); // a device mapping: there is no frame to wait on
    }
    let key = key_of(pa, va);
    let _table = TABLE.lock(R_WORDS);
    if crate::frame::phys::try_read_u32(pa) != Some(expected) {
        return Wait::Ret(E_AGAIN);
    }
    KEY[cur].store(key, Ordering::SeqCst);
    TICKET[cur].store(NEXT_TICKET.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    let deadline = if timeout_ticks == 0 {
        0
    } else {
        crate::timer::counter().saturating_add(timeout_ticks.saturating_mul(crate::timer::interval())).clamp(1, 1 << 55)
    };
    sched::set_state(cur, State::WordWait(deadline));
    Wait::Blocked
}

/// Does the word at `pa` lie in a frame of RAM (not in a device mapping, which `translate` also resolves)?
fn is_ram_word(pa: usize) -> bool {
    crate::frame::phys::is_ram_page(pa & !(crate::mem::PAGE - 1))
}

/// What a word is waited on by: its address in the program for a private page, its physical address (odd, which no
/// word address is) for memory that never moves and so can be shared.
fn key_of(pa: usize, va: usize) -> usize {
    if crate::mem::is_movable(pa) {
        va
    } else {
        pa | 1
    }
}

/// Wake up to `count` tasks waiting on `key` (of `space`'s program, or of any program for a shared word; the table lock is held).
fn wake_locked(space: usize, key: usize, count: usize) -> usize {
    let mut woken = 0;
    while woken < count {
        let mut best: Option<(u64, usize, u64)> = None;
        for m in (0..MAX_TASKS).filter(|&m| key & 1 != 0 || task::aspace(m) == space) {
            if let State::WordWait(d) = sched::state(m) {
                if KEY[m].load(Ordering::SeqCst) == key {
                    let ticket = TICKET[m].load(Ordering::SeqCst);
                    if best.map_or(true, |b| ticket < b.0) {
                        best = Some((ticket, m, d));
                    }
                }
            }
        }
        let Some((_, m, d)) = best else { break };
        frame(m).x[0] = 0; // the result of its WAIT_WORD, written before it can run
        KEY[m].store(0, Ordering::SeqCst);
        if sched::wake_if(m, State::WordWait(d)) {
            woken += 1;
        } // (else it was woken by something else — being condemned — meanwhile: not ours to count; look again)
    }
    woken
}

pub fn wake(cur: usize, va: usize, count: usize) -> i64 {
    if va % 4 != 0 {
        return 0;
    }
    let space = task::aspace(cur);
    let key = {
        let t = lock(space);
        let Some(pa) = crate::swap::translate(&t, space, va) else { return 0 };
        if !is_ram_word(pa) {
            return E_INVAL; // a device mapping: nothing can be waiting there
        }
        key_of(pa, va)
    };
    let _table = TABLE.lock(R_WORDS);
    wake_locked(space, key, count) as i64
}

/// The timeout of a `WAIT_WORD` that began with deadline `d` has passed (called by the scheduler).
pub fn expire(tid: usize, d: u64) {
    let _table = TABLE.lock(R_WORDS);
    if sched::state(tid) == State::WordWait(d) {
        frame(tid).x[0] = E_TIMEDOUT as u64;
        KEY[tid].store(0, Ordering::SeqCst);
        sched::wake_if(tid, State::WordWait(d));
    }
}

/// `THREAD_EXIT`'s store-and-wake (the program's task lock is held, via `t`). A word that cannot be reached is
/// ignored: the thread has ended either way.
pub fn store_and_wake(t: &task::Task, space: usize, va: usize, value: u32) {
    if va % 4 != 0 {
        return;
    }
    let Some(pa) = crate::swap::translate_writable(t, space, va) else { return }; // (a read-only page is never written for a program)
    if !is_ram_word(pa) {
        return; // a device mapping: nothing to store or wake
    }
    let key = key_of(pa, va);
    let _table = TABLE.lock(R_WORDS);
    if !crate::frame::phys::try_write_u32(pa, value) {
        return; // not RAM: nothing to store or wake
    }
    wake_locked(space, key, usize::MAX);
}
