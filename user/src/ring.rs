//! Userspace side of the async submission/completion rings (see kernel/src/ring.rs).
//! Posting submissions and reaping completions needs no syscall; `enter` is only
//! needed to hand a batch to the kernel (and optionally sleep for completions).
use crate::*;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

const RING_SETUP: u64 = 16;
const RING_ENTER: u64 = 17;
const SQ_N: u32 = 32;
const CQ_N: u32 = 32;
const SQ_OFF: usize = 64;
const CQ_OFF: usize = 1600;
const ENT: usize = 48;

pub const OP_NOP: u32 = 0;
pub const OP_SEND: u32 = 1;
pub const OP_CALL: u32 = 2;
pub const OP_TIMEOUT: u32 = 3;

pub struct Cqe {
    pub user_data: u64,
    pub res: i64,
    pub w: [u64; 3],
}

pub struct Ring {
    base: usize,
}

impl Ring {
    pub fn setup() -> Option<Ring> {
        let r = svc(RING_SETUP, 0, 0, 0, 0, 0)[0] as i64;
        (r > 0).then_some(Ring { base: r as usize })
    }

    fn at(&self, off: usize) -> &AtomicU32 {
        // SAFETY: `base` is the ring page the kernel mapped for RING_SETUP (non-zero, page-aligned, so 4-aligned); every offset used (< 1600 + 32 * 48 = 3136) is inside it; atomics because the kernel updates the indices concurrently.
        unsafe { AtomicU32::from_ptr((self.base + off) as *mut u32) }
    }

    /// Queue one submission. Returns false if the submission ring is full.
    pub fn submit(&self, op: u32, slot: u32, user_data: u64, w: [u64; 3]) -> bool {
        let tail = self.at(4).load(Ordering::Relaxed);
        if tail.wrapping_sub(self.at(0).load(Ordering::Acquire)) >= SQ_N {
            return false;
        }
        let e = self.base + SQ_OFF + (tail % SQ_N) as usize * ENT;
        // SAFETY: entry `tail % SQ_N` of the submission array (SQ_OFF + 32 * 48 = 1600) lies in the ring page; the kernel does not read it until the tail is published below.
        unsafe {
            write_volatile(e as *mut u32, op);
            write_volatile((e + 4) as *mut u32, slot);
            write_volatile((e + 8) as *mut u64, user_data);
            for (i, v) in w.iter().enumerate() {
                write_volatile((e + 16 + 8 * i) as *mut u64, *v);
            }
        }
        self.at(4).store(tail.wrapping_add(1), Ordering::Release); // publish
        true
    }

    /// Syscall: let the kernel take up to `to_submit` queued submissions, then sleep
    /// until at least `min_complete` completions are waiting. Returns how many were taken.
    pub fn enter(&self, to_submit: u32, min_complete: u32) -> i64 {
        svc(RING_ENTER, to_submit as u64, min_complete as u64, 0, 0, 0)[0] as i64
    }

    /// Pop one completion, if any. No syscall.
    pub fn reap(&self) -> Option<Cqe> {
        let head = self.at(8).load(Ordering::Relaxed);
        if head == self.at(12).load(Ordering::Acquire) {
            return None;
        }
        let e = self.base + CQ_OFF + (head % CQ_N) as usize * ENT;
        // SAFETY: entry `head % CQ_N` of the completion array (CQ_OFF + 32 * 48 = 3136 < 4096) is in the ring page; the kernel published it before the tail we acquired above.
        let c = unsafe {
            Cqe {
                user_data: read_volatile(e as *const u64),
                res: read_volatile((e + 8) as *const i64),
                w: [read_volatile((e + 16) as *const u64), read_volatile((e + 24) as *const u64), read_volatile((e + 32) as *const u64)],
            }
        };
        self.at(8).store(head.wrapping_add(1), Ordering::Release);
        Some(c)
    }
}

const OP_PING: u64 = 10;
const OP_CRASH: u64 = 11;

pub fn ring_tests() {
    let Some(r) = Ring::setup() else {
        println!("[client] TEST async ring setup: FAIL");
        return;
    };
    let again = Ring::setup().is_none();
    println!("[client] TEST async ring setup (second setup refused={again}): {}", verdict(again));

    // 1. a batch submitted and completed with a single syscall
    for i in 0..3 {
        r.submit(OP_NOP, 0, 0x100 + i, [0; 3]);
    }
    let n = r.enter(3, 3);
    let mut order = [0u64; 3];
    for o in order.iter_mut() {
        *o = r.reap().map_or(0, |c| c.user_data);
    }
    println!("[client] TEST ring batch: 3 NOPs, 1 syscall (submitted={n}, completions {order:x?}): {}",
        verdict(n == 3 && order == [0x100, 0x101, 0x102] && r.reap().is_none()));

    // 2. genuinely concurrent calls: 4 pings to the driver + a 20-tick timeout, one syscall
    let (_, base) = call(2, [OP_PING, 0, 0]);
    for i in 0..4 {
        r.submit(OP_CALL, 2, 0x200 + i, [OP_PING, 0, 0]);
    }
    r.submit(OP_TIMEOUT, 0, 0x2FF, [20, 0, 0]);
    let t0 = ticks();
    let n = r.enter(5, 5);
    let dt = ticks() - t0;
    let (mut served, mut seen, mut last, mut ok) = (Vec::new(), 0u32, 0u64, true);
    while let Some(c) = r.reap() {
        ok &= c.res == 0;
        seen |= 1 << (c.user_data & 0xFF).min(31);
        last = c.user_data;
        if c.user_data != 0x2FF {
            served.push(c.w[0]);
        }
    }
    served.sort();
    println!("[client] TEST ring: 4 concurrent async calls + timeout, 1 syscall (submitted={n}, served={served:?}, waited {dt} ticks): {}",
        verdict(ok && n == 5 && last == 0x2FF && seen.count_ones() == 5 && served == [base[0] + 1, base[0] + 2, base[0] + 3, base[0] + 4] && dt >= 20));

    // 3. poll the completion ring without any syscall
    // (Polling against an Interactive server: on one core a spinning Interactive task would
    // starve a Background one, as there is no priority inheritance.)
    r.submit(OP_SEND, 0, 0x300, [3, 77, 0]); // notification: completes as soon as it is queued
    r.submit(OP_CALL, 0, 0x301, [OP_PING, 0, 0]); // service: unknown op -> error reply, still a completion
    r.enter(2, 0);
    let (mut got, mut spins, mut sums) = (0, 0u64, 0u64);
    while got < 2 && spins < 50_000_000 {
        if let Some(c) = r.reap() {
            got += 1;
            sums += c.user_data;
        }
        spins += 1;
        core::hint::spin_loop();
    }
    println!("[client] TEST ring: completions reaped by polling, no wait syscall (got {got}, {spins} polls): {}",
        verdict(got == 2 && sums == 0x300 + 0x301));

    // 3b. priority inheritance: poll for an async call to a *Background* server while this
    // Interactive task spins. On a single core the server only runs if it inherits our priority.
    r.submit(OP_CALL, 2, 0x350, [OP_PING, 0, 0]);
    r.enter(1, 0);
    let (mut spins, mut got) = (0u64, false);
    while !got && spins < 30_000_000 {
        got = r.reap().map_or(false, |c| c.user_data == 0x350 && c.res == 0);
        spins += 1;
        core::hint::spin_loop();
    }
    println!("[client] TEST priority inheritance: polled async call to a Background server completed ({spins} polls): {}", verdict(got));

    // 4. a server that dies with an async call in flight: error completion, then a restarted server
    r.submit(OP_CALL, 2, 0x400, [OP_CRASH, 0, 0]);
    r.submit(OP_CALL, 2, 0x401, [OP_PING, 0, 0]);
    r.enter(2, 2);
    let (mut dead, mut served1) = (0i64, 0u64);
    while let Some(c) = r.reap() {
        match c.user_data {
            0x400 => dead = c.res,
            0x401 => served1 = if c.res == 0 { c.w[0] } else { 0 },
            _ => {}
        }
    }
    println!("[client] TEST ring: async call to a crashing driver completes with an error (res={dead}), next call served by the restarted one (served={served1}): {}",
        verdict(dead == E_DEAD && served1 == 1));

    // 5. backpressure: SQ holds 32; a full CQ of completions is drained before more are accepted
    let mut queued = 0;
    while r.submit(OP_NOP, 0, 0x500 + queued, [0; 3]) {
        queued += 1;
    }
    let n1 = r.enter(64, 0);
    let mut drained = 0;
    while r.reap().is_some() {
        drained += 1;
    }
    println!("[client] TEST ring backpressure (SQ accepted {queued}, kernel took {n1}, reaped {drained}): {}",
        verdict(queued == 32 && n1 == 32 && drained == 32));
}
