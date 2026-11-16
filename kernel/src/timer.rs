//! Architected EL1 physical timer: one per core, one-shot. Time is the free-running system
//! counter; a core's timer is armed for the earliest of its next 100 Hz tick, the end of the
//! running task's time slice and the next real-time event on that core (see `sched.rs`).
//!
//! `ticks()` is the counter divided into 10 ms jiffies, so it advances whether or not any core
//! has taken its tick yet. Each core takes its own jiffy interrupt (phase-shifted so the cores
//! do not all enter the kernel at once); core 0's drives the async-call timeouts.
use crate::smp::MAX_CPUS;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub const TIMER_PPI: u32 = 30;
pub const HZ: u64 = 100;

/// Counter ticks per second.
static FREQ: AtomicU64 = AtomicU64::new(0);
/// Counter ticks per jiffy.
static INTERVAL: AtomicU64 = AtomicU64::new(0);
/// Counter value at boot (jiffy 0).
static BASE: AtomicU64 = AtomicU64::new(0);
/// Counter value of each core's next jiffy interrupt.
static NEXT_JIFFY: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// What each core's timer is programmed for. Writing the timer is not free (on an emulator it is
/// dear, and every core's write is serialised), so `arm` writes only when an earlier interrupt
/// is needed than the one already pending; a stale early one costs a harmless extra interrupt.
static ARMED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The system counter. (User space reads the same counter as `cntvct_el0`; the virtual offset is zero.)
#[inline]
pub fn counter() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

pub fn freq() -> u64 {
    FREQ.load(Ordering::Relaxed)
}

pub fn interval() -> u64 {
    INTERVAL.load(Ordering::Relaxed)
}

pub fn us_to_counter(us: u64) -> u64 {
    us.saturating_mul(freq()) / 1_000_000
}

pub fn counter_to_us(c: u64) -> u64 {
    let f = freq();
    (c / f) * 1_000_000 + (c % f) * 1_000_000 / f
}

/// Monotonic time since the counter started, in nanoseconds.
pub fn now_ns() -> u64 {
    let (c, f) = (counter() as u128, freq().max(1) as u128);
    (c * 1_000_000_000 / f) as u64
}

/// `wall = WALL_OFFSET_NS + now_ns()`, set once at boot from the real-time clock (0 = the platform has none).
static WALL_OFFSET_NS: AtomicU64 = AtomicU64::new(0);

/// Set the wall clock to `unix_ns` nanoseconds since 1970 (from network time): the offset to the monotonic clock is what is stored.
pub fn set_wall(unix_ns: u64) {
    WALL_OFFSET_NS.store(unix_ns.saturating_sub(now_ns()).max(1), Ordering::Relaxed);
}

/// The wall clock in nanoseconds since 1970, if the platform has a real-time clock.
pub fn wall_ns() -> Option<u64> {
    match WALL_OFFSET_NS.load(Ordering::Relaxed) {
        0 => None,
        off => Some(off + now_ns()),
    }
}

/// Boot core: global GIC state and the tick interval.
pub fn init() {
    crate::gic::init_dist();
    let freq: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) freq) };
    FREQ.store(freq, Ordering::Relaxed);
    INTERVAL.store(freq / HZ, Ordering::Relaxed);
    BASE.store(counter(), Ordering::Relaxed);
    if let Some(s) = crate::platform::rtc_seconds() {
        WALL_OFFSET_NS.store((s * 1_000_000_000).saturating_sub(now_ns()).max(1), Ordering::Relaxed);
    }
    init_core();
}

/// Every core: its own GIC interface and timer.
pub fn init_core() {
    crate::gic::init_cpu();
    // let EL0 read the physical/virtual counter (user-space benchmarks time themselves with it)
    unsafe { asm!("msr cntkctl_el1, {}", in(reg) 3u64) };
    let core = crate::smp::cpu_id();
    let (base, iv) = (BASE.load(Ordering::Relaxed), interval());
    let phase = iv / MAX_CPUS as u64 * core as u64;
    let k = (counter() - base) / iv + 1;
    NEXT_JIFFY[core].store(base + k * iv + phase, Ordering::Relaxed);
    unsafe { asm!("msr cntp_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack)) };
    program(core, NEXT_JIFFY[core].load(Ordering::Relaxed));
}

fn program(core: usize, at: u64) {
    ARMED[core].store(at, Ordering::Relaxed);
    unsafe { asm!("msr cntp_cval_el0, {}", in(reg) at, options(nomem, nostack)) };
}

/// An idle core's timer: set for `event` (a counter value; 0 = never) and *not* for the next jiffy,
/// so a core with nothing to do and nothing to wait for is not woken a hundred times a second.
pub fn arm_idle(core: usize, event: u64) {
    program(core, if event == 0 { u64::MAX } else { event });
}

/// The core has been running (or sleeping without a tick): move its jiffy schedule past `now`, so the
/// next arm does not set the timer in the past.
pub fn resync(core: usize, now: u64) {
    let (iv, next) = (interval(), NEXT_JIFFY[core].load(Ordering::Relaxed));
    if now >= next {
        NEXT_JIFFY[core].store(next + ((now - next) / iv + 1) * iv, Ordering::Relaxed);
    }
}

/// Make sure this core's timer fires no later than `event` (a counter value; 0 = no event)
/// or than its next jiffy.
pub fn arm(core: usize, event: u64) {
    let mut at = NEXT_JIFFY[core].load(Ordering::Relaxed);
    if event != 0 && event < at {
        at = event;
    }
    if ARMED[core].load(Ordering::Relaxed) > at {
        program(core, at);
    }
}

pub fn ticks() -> u64 {
    (counter() - BASE.load(Ordering::Relaxed)) / INTERVAL.load(Ordering::Relaxed)
}

/// The timer fired on `core`. Returns whether a jiffy boundary was crossed (as opposed to an early
/// one-shot for a slice end or real-time event). Re-arms for the next jiffy so the line goes quiet;
/// the dispatcher then arms the precise event.
pub fn on_irq(core: usize) -> bool {
    let (now, iv) = (counter(), interval());
    let next = NEXT_JIFFY[core].load(Ordering::Relaxed);
    let jiffy = now >= next;
    if jiffy {
        NEXT_JIFFY[core].store(next + ((now - next) / iv + 1) * iv, Ordering::Relaxed);
    }
    program(core, NEXT_JIFFY[core].load(Ordering::Relaxed));
    jiffy
}
