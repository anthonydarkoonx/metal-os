#![no_std]
#![no_main]
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::*};
use metal_programs::thread;
use metal_programs::*;

// Regression tests for the two races found by the kernel audit (SECURITY.md, H1 and H2). Each needs two cores and is a
// race, so a pass means "not seen in these runs"; the test is written so that the unfixed kernel fails it quickly.
//
//   1  H1: capability checks against revocation. A thread maps a shared-memory capability over and over while the main
//      thread revokes (and so frees) the object. If a map can complete after the revoke, a mapping of freed frames is left
//      behind; the main thread finds it because a fresh object cannot then be mapped at the same address (-5).
//      Exit 0 = no stale mapping in all rounds; 10 = one was left behind; other = setup failed.
//   2  H2: HEAP_RELEASE against the TLB. A thread keeps reading the first page of a heap range that the main thread gives
//      back. Another program (mode 3) is allocating and filling pages at the same time. If frames are freed before the TLB
//      is flushed, the thread's stale translation shows the other program's data. Prints "LEAK" when it does. The thread
//      ends up reading an unmapped page once the release is over, so this program is expected to fault: only the output
//      counts.
//   3  the allocating program for mode 2: waits for the main thread's signal (word 0 of the shared object in slot 0), then takes
//      and fills pages.

//   4, 5, 6  M1: run-time memory (MC_ALLOC) and a thread's copy of its capability. The thread ends the object, in three ways, through
//      its copy: 4 = CAP_REVOKE of itself, 5 = the thread simply ends, 6 = MC_FREE. The object must stay usable for the main
//      thread (whose capability it is): a second mapping works, and the first one still holds what was written. Exit 0 = it did.
//      (A MemCtl capability is in slot 1.)

//   7  M2: CAP_GRANT goes only where it is wanted. The argument's upper bits name another program (a sibling, mode 8). Grants of a
//      memory capability to a boot-time task (task 2), to the sibling (first free slot, and a named slot) must be refused (-1); a
//      grant to the parent must work. Exit code bits: 1 boot-time task not refused, 2 sibling not refused, 4 named slot in the
//      sibling not refused, 8 parent refused.
//   9  L1: SYSINFO pages 10, 11, 13 and 14 (what other tasks hold) need an Admin capability, which this program has not: each must
//      answer -1, while page 0 works. Exit code: one bit per page that was not refused (1, 2, 4, 8), 16 if page 0 failed.
//   10 M4: a floating-point / SIMD instruction in a user program traps and ends it (the registers are not switched between tasks, so
//      they must not be usable). Prints "survived" if it ran; the program is expected to fault.
//   11 M4: TPIDR_EL0 belongs to the task: this one sets A, a thread sets B and gives up the CPU many times, and a new thread starts
//      with 0. Exit code bits: 1 the thread did not start with 0, 2 the thread lost its own value, 4 this task's value was changed.
//   12 M7: one program's run-time memory objects (SHM_CREATE) are limited, however many threads make them: seven threads and this one
//      create until refused. Exit code bits: 1 more than PROGRAM_OBJ_QUOTA (32) were made, 2 none were.
//   13 M7: one program's capability nodes are limited: this one holds 31 capabilities and starts seven threads that each copy all of
//      them. Exit code bits: 1 all seven threads were started (no limit), 2 none was.
//   14 M7: one program's run-time endpoints are limited, however many threads make them (a factory capability is in slot 1): seven
//      threads and this one create until refused. Exit code bits: 1 more than 4 were made, 2 none were.
//   15 M7: one program may not fill an endpoint's queue (a factory capability is in slot 1): it makes an endpoint, queues messages
//      to it until refused, and a thread of the same program then tries one more. Exit code bits: 1 not exactly 4 were queued
//      (half of the queue of 8), 2 the thread's message was accepted.
//   8  the sibling: sleeps for a while, then ends.

const BIG: usize = 0xA000_0000;
const MEMCTL: u64 = 18;
const MC_ALLOC: u64 = 2;
const MC_FREE: u64 = 3;
const PAGE: usize = 4096;
const E_BADCAP: i64 = -2;
const HEAP_RELEASE: u64 = 19;
const CAP_GRANT: u64 = 5;
/// The slot the thread receives the object in (the argument of CAP_GRANT is slot + 1).
const THEIR_SLOT: u64 = 20;

const ROUNDS: u32 = 3000;
const T_MAGIC: u64 = 0x7E57_7E57_7E57_7E57;
const U_MAGIC: u64 = 0xA110_CA7E_D0DE_C0DE;
const RELEASE_PAGES: usize = 2048;
const ALLOC_PAGES: usize = 4096;

static GEN: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicU32 = AtomicU32::new(0);
static GO: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);
static WATCH: AtomicUsize = AtomicUsize::new(0);

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// H1's mapper: for each round, once the main thread has handed it the object, map it until the capability is gone.
extern "C" fn mapper(_: u64) -> ! {
    let mut seen = 0;
    loop {
        let g = loop {
            if STOP.load(Acquire) != 0 {
                thread::exit(&DONE);
            }
            let g = GEN.load(Acquire);
            if g != seen {
                break g;
            }
            core::hint::spin_loop();
        };
        seen = g;
        for _ in 0..200_000 {
            if map(THEIR_SLOT, BIG, true) == E_BADCAP {
                break;
            }
        }
        DONE.store(g + 1000, Release); // (not the word the kernel stores into when the thread ends)
    }
}

fn h1() -> u64 {
    let id = thread::create_with_kernel_stack(mapper, 0, 0, 0);
    if id < 0 {
        return 2;
    }
    let mut r = 0x9E37_79B9_7F4A_7C15u64;
    for round in 1..=ROUNDS {
        let s = shm_create(1);
        if s < 0 {
            return 3;
        }
        let s = s as u64;
        if svc(CAP_GRANT, s, id as u64, 0xFFFF & !4, THEIR_SLOT + 1, 0)[0] as i64 != THEIR_SLOT as i64 {
            return 4;
        }
        GEN.store(round, Release);
        for _ in 0..(rng(&mut r) % 400) {
            core::hint::spin_loop(); // (a different point of the mapper's loop each time)
        }
        revoke_self(s);
        while DONE.load(Acquire) != round + 1000 {
            core::hint::spin_loop();
        }
        // The mapper is done and the object gone: nothing may be left mapped at BIG. A fresh object maps there only if so.
        let s2 = shm_create(1);
        if s2 < 0 {
            return 5;
        }
        let m = map(s2 as u64, BIG, true);
        revoke_self(s2 as u64); // (takes its mapping with it)
        if m != 0 {
            println!("racetest: STALE mapping left behind in round {round} (map of a fresh object returned {m})");
            return 10;
        }
    }
    STOP.store(1, Release);
    0
}

/// H2's reader: reads the first word of the range that is about to be released, as fast as it can.
extern "C" fn watcher(_: u64) -> ! {
    while GO.load(Acquire) == 0 {
        core::hint::spin_loop();
    }
    let p = WATCH.load(Relaxed) as *const u64;
    loop {
        // SAFETY: the main thread keeps this page mapped until it releases it; reading it afterwards faults, which ends the program, and this test expects that.
        let v = unsafe { core::ptr::read_volatile(p) };
        if v == U_MAGIC && LEAK.swap(1, Relaxed) == 0 {
            println!("racetest: LEAK, a released page shows another program's data ({v:#x})");
        }
    }
}

/// A word of the shared object both H2 programs hold in slot 0 (the conductor made it): the main thread sets it just before
/// the release, the allocator waits for it.
fn sync_word() -> &'static AtomicU32 {
    // SAFETY: slot 0 holds a 1-page shared object, mapped at BIG by `attach`; this program keeps it mapped to the end.
    unsafe { &*(BIG as *const AtomicU32) }
}

fn attach() -> bool {
    map(0, BIG, true) == 0
}

fn h2() -> u64 {
    if !attach() {
        return 1;
    }
    let base = heap_grow(RELEASE_PAGES as u64);
    if base < 0 {
        return 2;
    }
    let base = base as usize;
    for p in 0..RELEASE_PAGES {
        // SAFETY: `heap_grow` just mapped RELEASE_PAGES writable pages at `base`.
        unsafe { core::ptr::write_volatile((base + p * PAGE) as *mut u64, T_MAGIC) };
    }
    WATCH.store(base, Relaxed);
    if thread::create_with_kernel_stack(watcher, 0, 1 << 7, 0) < 0 /* (slot 7: where the console output goes) */ {
        return 3;
    }
    GO.store(1, Release);
    sleep(2); // let the watcher get its translation and run on its core
    sync_word().store(1, Release); // the allocator starts now
    let freed = svc(HEAP_RELEASE, 0, RELEASE_PAGES as u64, 0, 0, 0)[0] as i64;
    println!("racetest: released {freed} pages");
    0
}

/// The other program: waits for the release to start, then takes and fills as many pages as it can.
fn allocator() -> u64 {
    if !attach() {
        return 1;
    }
    while sync_word().load(Acquire) == 0 {
        core::hint::spin_loop();
    }
    let mut taken = 0;
    for _ in 0..ALLOC_PAGES {
        let b = heap_grow(1);
        if b < 0 {
            break;
        }
        // SAFETY: `heap_grow(1)` just mapped a writable page at `b`.
        unsafe { core::ptr::write_volatile(b as *mut u64, U_MAGIC) };
        taken += 1;
    }
    println!("racetest: allocator took {taken} pages");
    (taken != ALLOC_PAGES) as u64 // 0 = it got all it asked for while the release was running
}

static MODE: AtomicU32 = AtomicU32::new(0);
static TP_RESULT: AtomicU32 = AtomicU32::new(0);

fn tpidr_read() -> u64 {
    let v: u64;
    // SAFETY: reads TPIDR_EL0, which a program may use as it likes.
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) v, options(nomem, nostack)) };
    v
}

fn tpidr_write(v: u64) {
    // SAFETY: writes TPIDR_EL0, which a program may use as it likes.
    unsafe { core::arch::asm!("msr tpidr_el0, {}", in(reg) v, options(nomem, nostack)) };
}

extern "C" fn tp_thread(_: u64) -> ! {
    let mut bits = 0;
    if tpidr_read() != 0 {
        bits |= 1;
    }
    tpidr_write(0xB0B0_B0B0);
    for _ in 0..3000 {
        yield_now();
        if tpidr_read() != 0xB0B0_B0B0 {
            bits |= 2;
        }
    }
    TP_RESULT.store(bits, Release);
    thread::exit(&DONE)
}

fn tpidr() -> u64 {
    tpidr_write(0xA0A0_A0A0);
    if thread::create_with_kernel_stack(tp_thread, 0, 0, 0) < 0 {
        return 100;
    }
    let mut bits = 0;
    for _ in 0..3000 {
        yield_now();
        if tpidr_read() != 0xA0A0_A0A0 {
            bits |= 4;
        }
    }
    thread::join(&DONE);
    bits | TP_RESULT.load(Acquire) as u64
}

/// M1's thread: it holds copies of the MemCtl capability (slot 1) and of the object's (slot `s`).
extern "C" fn copier(s: u64) -> ! {
    match MODE.load(Acquire) {
        4 => {
            revoke_self(s);
        }
        6 => {
            svc(MEMCTL, 1, MC_FREE, s, 0, 0);
        }
        _ => {} // 5: it just ends
    }
    thread::exit(&DONE)
}

fn m1(mode: u64) -> u64 {
    let s = svc(MEMCTL, 1, MC_ALLOC, 1, 0, 0)[0] as i64;
    if s < 0 {
        return 2;
    }
    let s = s as u64;
    if map(s, BIG, true) != 0 {
        return 3;
    }
    // SAFETY: the 1-page object was just mapped writable at BIG.
    unsafe { core::ptr::write_volatile(BIG as *mut u64, T_MAGIC) };
    MODE.store(mode as u32, Release);
    if thread::create_with_kernel_stack(copier, s, (1 << 1) | (1 << s), 0) < 0 {
        return 4;
    }
    thread::join(&DONE);
    if map(s, BIG + 0x10000, true) != 0 {
        return 10; // the object is gone from under the capability that is still here
    }
    // SAFETY: BIG and BIG + 0x10000 are mappings of the same live object.
    if unsafe { core::ptr::read_volatile((BIG + 0x10000) as *const u64) } != T_MAGIC {
        return 11;
    }
    0
}

fn m2(sibling: u64, parent: u64) -> u64 {
    let s = shm_create(1);
    if s < 0 {
        return 100;
    }
    let s = s as u64;
    let mut bits = 0;
    let grant = |to: u64, dest: u64| svc(CAP_GRANT, s, to, 0xFFFF & !4, dest, 0)[0] as i64;
    if grant(2, 0) != -1 {
        bits |= 1;
    }
    if grant(sibling, 0) != -1 {
        bits |= 2;
    }
    if grant(sibling, 8) != -1 {
        bits |= 4;
    }
    if grant(parent, 0) < 0 {
        bits |= 8;
    }
    revoke_self(s); // (takes the copy in the parent with it)
    bits
}

const T_COUNT: usize = 7;
static DONES: [AtomicU32; T_COUNT] = [const { AtomicU32::new(0) }; T_COUNT];
static MADE: AtomicU32 = AtomicU32::new(0);
/// How many threads have finished making things, and the signal for them to end (what a thread made goes with it when it ends).
static ARRIVED: AtomicU32 = AtomicU32::new(0);
static RELEASE: AtomicU32 = AtomicU32::new(0);

extern "C" fn maker(i: u64) -> ! {
    match MODE.load(Acquire) {
        12 => {
            while shm_create(1) >= 0 {
                MADE.fetch_add(1, SeqCst);
            }
        }
        14 => {
            while svc(27, 1, 0, 0, 0, 0)[0] as i64 >= 0 {
                MADE.fetch_add(1, SeqCst);
            }
        }
        _ => {} // 13: the copies are the test
    }
    ARRIVED.fetch_add(1, SeqCst);
    while RELEASE.load(Acquire) == 0 {
        thread::wait_word(&RELEASE, 0, 0);
    }
    thread::exit(&DONES[i as usize])
}

fn m7(mode: u64, limit: u32) -> u64 {
    MODE.store(mode as u32, Release);
    let mut held = 0;
    if mode == 13 {
        // (fill the table of capabilities: the slots the program was given are taken already)
        while shm_create(1) >= 0 {
            held += 1;
        }
        if held < 16 {
            return 90; // (the setup: capabilities to copy)
        }
    }
    let mut started = 0;
    for i in 0..T_COUNT {
        let mask = if mode == 13 { u32::MAX & !(1 << 31) } else { 1 << 1 };
        if thread::create_with_kernel_stack(maker, i as u64, mask, 0) >= 0 {
            started += 1;
        } else {
            DONES[i].store(1, Release);
        }
    }
    if mode != 13 {
        // this thread makes its share too
        loop {
            let ok = if mode == 12 { shm_create(1) >= 0 } else { svc(27, 1, 0, 0, 0, 0)[0] as i64 >= 0 };
            if !ok {
                break;
            }
            MADE.fetch_add(1, SeqCst);
        }
    }
    // (what a thread made is gone when it ends: count while they all still stand)
    while ARRIVED.load(Acquire) < started {
        sleep(1);
    }
    let made = MADE.load(SeqCst);
    RELEASE.store(1, Release);
    thread::wake_word(&RELEASE, T_COUNT);
    for d in DONES.iter() {
        thread::join(d);
    }
    let mut bits = 0;
    if mode == 13 {
        if started == T_COUNT as u32 {
            bits |= 1;
        }
        if started == 0 {
            bits |= 2;
        }
    } else {
        if made > limit {
            bits |= 1;
        }
        if made == 0 {
            bits |= 2;
        }
    }
    bits
}

fn queue_share() -> u64 {
    let ep = svc(27, 1, 0, 0, 0, 0)[0] as i64;
    if ep < 0 {
        return 90;
    }
    let mut n = 0;
    while svc(1, ep as u64, 0, 0, 0, 0)[0] as i64 == 0 && n < 16 {
        n += 1;
    }
    let mut bits = if n == 4 { 0 } else { 1 };
    MODE.store(15, Release);
    EPSLOT.store(ep as u32, Release);
    if thread::create_with_kernel_stack(sender, 0, 1 << ep, 0) < 0 {
        return 91;
    }
    thread::join(&DONES[0]);
    if THREAD_SENT.load(Acquire) != 0 {
        bits |= 2;
    }
    bits
}
static EPSLOT: AtomicU32 = AtomicU32::new(0);
static THREAD_SENT: AtomicU32 = AtomicU32::new(0);

extern "C" fn sender(_: u64) -> ! {
    if svc(1, EPSLOT.load(Acquire) as u64, 0, 0, 0, 0)[0] as i64 == 0 {
        THREAD_SENT.store(1, Release);
    }
    thread::exit(&DONES[0])
}

program!("racetest", |c| {
    match c.arg & 0xFF {
        8 => {
            sleep(100);
            0
        }
        9 => {
            let mut bits = 0;
            for (i, page) in [10u64, 11, 13, 14].iter().enumerate() {
                if svc(21, *page, 0, 0, 0, 0)[0] as i64 != -1 {
                    bits |= 1 << i;
                }
            }
            if svc(21, 0, 0, 0, 0, 0)[0] as i64 != 0 {
                bits |= 16;
            }
            bits
        }
        7 => m2(c.arg >> 8, c.parent),
        10 => {
            // SAFETY: `.inst` emits `movi v0.16b, #0`, which touches only a vector register; the kernel is expected to refuse it.
            unsafe { core::arch::asm!(".inst 0x4f000400", out("v0") _) };
            println!("racetest: survived a SIMD instruction");
            0
        }
        11 => tpidr(),
        12 => m7(12, 32),
        13 => m7(13, 0),
        14 => m7(14, 4),
        15 => queue_share(),
        4..=6 => m1(c.arg),
        1 => h1(),
        2 => h2(),
        3 => allocator(),
        _ => 99,
    }
});
