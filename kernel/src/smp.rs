//! Symmetric multiprocessing: core bring-up through PSCI and idle-core wakeups by IPI.
//!
//! There is no global kernel lock: kernel code on different cores runs in parallel and
//! synchronises through the fine-grained locks described in `sync.rs`. A core with nothing
//! to run sleeps in `wfi` with no lock held; whoever makes work runnable sends it an IPI, and a busy
//! core that runs something less important than a task that just became runnable gets one too.
use crate::sync::Slots;
use crate::trap::TrapFrame;
use crate::{gic, kprintln, mmu, sched, timer};
use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// The most cores the kernel runs: the tables sized by it are small, and the stacks are made from RAM when a core starts (the boot core's is in the image).
/// The machine's own number comes from its device tree (`platform::cpu_count`). Masks of cores are 32 bits wide.
pub const MAX_CPUS: usize = 32;
const SGI_WAKE: u32 = 0;

/// Cores sleeping in `idle()`.
static IDLE_MASK: AtomicU32 = AtomicU32::new(0);
/// Cores that have completed bring-up (bit 0 = boot core).
static ONLINE: AtomicU32 = AtomicU32::new(1);

/// This core's number. Kept in `tpidr_el1` (set once at bring-up): reading it is a plain register
/// read, while `mpidr_el1` is a call into the emulator, and this is asked on every scheduling step.
pub fn cpu_id() -> usize {
    let m: u64;
    // SAFETY: reads TPIDR_EL1, which boot code set to this core's number; no memory is accessed.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) m, options(nomem, nostack)) };
    m as usize
}

/// Is `core` asleep in `idle()`?
pub fn is_idle(core: usize) -> bool {
    IDLE_MASK.load(Ordering::SeqCst) & (1 << core) != 0
}

pub fn online_mask() -> u32 {
    ONLINE.load(Ordering::Acquire)
}

/// Sleep this core until an interrupt. Called with no lock held.
pub fn idle() {
    // An idle core must not keep a task's address space loaded: that task may die on another
    // core, and its page-table frames be freed and reused while this core still walks them.
    mmu::use_kernel_tables();
    let core = cpu_id();
    let me = 1 << core;
    // Announce first, then look for work once more: a task made runnable between the caller's
    // last look and now is then either seen here, or its waker sees us in the idle mask and
    // sends the IPI. (Either way no wakeup is lost.)
    IDLE_MASK.fetch_or(me, Ordering::SeqCst);
    if sched::has_work_for(core) {
        IDLE_MASK.fetch_and(!me, Ordering::SeqCst);
        return;
    }
    // SAFETY: WFI then a momentary IRQ unmask/mask; no lock or kernel borrow is held (see the doc), so a taken interrupt handler cannot
    // re-enter held state. DAIF I is masked again before returning.
    unsafe {
        // wfi wakes on a *pending* interrupt even while masked, so an IPI sent just before
        // this point cannot be lost. Then unmask just long enough to take it.
        asm!("wfi", "msr daifclr, #2", "isb", "msr daifset, #2", options(nostack));
    }
    IDLE_MASK.fetch_and(!me, Ordering::SeqCst);
}

/// Wake the sleeping cores that have something they could run.
pub fn kick_idle() {
    if !sched::take_dirty() {
        return;
    }
    let idle = IDLE_MASK.load(Ordering::SeqCst);
    let (wake, preempt) = sched::resched_targets(idle);
    if wake != 0 {
        // Claim the wakeup: until the sleeper runs again it must not look idle, or every
        // other core's next kernel entry would send it the same interrupt again.
        IDLE_MASK.fetch_and(!wake, Ordering::SeqCst);
    }
    if wake | preempt != 0 {
        gic::send_sgi_to(wake | preempt, SGI_WAKE);
    }
}

extern "C" {
    fn _secondary_start();
    fn enter_user(tf: *const TrapFrame) -> !;
}

static BOOT_FRAMES: Slots<TrapFrame, MAX_CPUS> = Slots::new([TrapFrame::ZERO; MAX_CPUS]);

/// The top of each core's kernel stack and of its emergency stack (the one a stack overflow is reported on): `boot.S` reads these with the MMU on, for
/// every core but the boot core, whose two are in the image.
#[no_mangle]
pub static CORE_STACK_TOP: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
#[no_mangle]
pub static CORE_EMERG_TOP: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Where each core's stack guard starts (`crate::mmu::KSTACK_GUARD` bytes, unmapped); 0 for a core that has none yet.
pub static GUARD_START: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// Pages of a core's memory: its emergency stack, the guard, then the stack (which grows down towards the guard).
const EMERG_PAGES: usize = crate::mmu::KSTACK_EMERGENCY / crate::mem::PAGE;
const GUARD_PAGES: usize = crate::mmu::KSTACK_GUARD / crate::mem::PAGE;
const STACK_PAGES: usize = crate::mmu::KSTACK_SIZE / crate::mem::PAGE;

/// Make `core`'s stacks out of RAM (the guard unmapped): the stack top. `None` if there is no room.
fn make_stacks(core: usize) -> Option<usize> {
    let pages = EMERG_PAGES + GUARD_PAGES + STACK_PAGES;
    let base = crate::mem::alloc_contig(pages, crate::mem::OWNER_KERNEL)?;
    // (the boot core zeroed the pages with its caches on; the new core starts with its MMU off, writes straight to memory and reads through its cache after it is on:
    // the zeros must be in memory and no copy of them left in this core's cache)
    crate::cache::clean(base, pages * crate::mem::PAGE);
    crate::cache::invalidate(base, pages * crate::mem::PAGE);
    let guard = base + EMERG_PAGES * crate::mem::PAGE;
    if !crate::mmu::unmap_kernel_pages(guard, crate::mmu::KSTACK_GUARD) {
        for p in 0..pages {
            crate::mem::free(base + p * crate::mem::PAGE);
        }
        return None;
    }
    let top = guard + crate::mmu::KSTACK_GUARD + crate::mmu::KSTACK_SIZE;
    CORE_STACK_TOP[core].store(top, Ordering::Relaxed);
    CORE_EMERG_TOP[core].store(guard, Ordering::Relaxed);
    GUARD_START[core].store(guard, Ordering::Relaxed);
    Some(top)
}

/// PSCI CPU_ON (the platform knows the conduit and how a core is numbered): the core starts at `_secondary_start` with its stack top and number as the context id.
fn psci_cpu_on(core: usize, stack_top: usize) -> i64 {
    // SAFETY: `_secondary_start` is the secondary cores' entry point.
    unsafe { crate::platform::psci_cpu_on(core, _secondary_start as *const () as usize as u64, (stack_top | core) as u64) }
}

/// Boot core: power up whatever other cores exist.
pub fn start_secondaries() -> usize {
    // (the device tree says how many cores there are; without one the firmware is asked about each in turn)
    let want = crate::platform::cpu_count().unwrap_or_else(|| (1..MAX_CPUS).find(|&c| !crate::platform::psci_core_exists(c)).unwrap_or(MAX_CPUS)).min(MAX_CPUS);
    // Every core's stacks first, while this is the only core running: making a stack guard can replace a 2 MiB block of the identity map (`mmu::unmap_kernel_pages`).
    let mut tops = [0usize; MAX_CPUS];
    let mut cores = want;
    for core in 1..want {
        match make_stacks(core) {
            Some(t) => tops[core] = t,
            None => {
                kprintln!("[smp] no memory for core {core}'s stack");
                cores = core;
                break;
            }
        }
    }
    let mut started = 0;
    for core in 1..cores {
        match psci_cpu_on(core, tops[core]) {
            0 => started += 1,
            -2 => break, // INVALID_PARAMETERS: no such core
            e => {
                kprintln!("[smp] PSCI CPU_ON({core}) failed: {e}");
                break;
            }
        }
    }
    crate::platform::set_cpu_count_if_unknown(started + 1); // (placement and the timer phase use the same count as the cores that came up)
    started
}

/// Entry for each secondary core, called from boot.S with the MMU off.
#[no_mangle]
extern "C" fn secondary_main(core: usize) -> ! {
    // SAFETY: writes TPIDR_EL1 with this core's number, once at bring-up; only `cpu_id` reads it; no memory is accessed.
    unsafe { asm!("msr tpidr_el1, {}", in(reg) core as u64, options(nomem, nostack)) };
    mmu::enable_this_cpu();
    timer::init_core();
    ONLINE.fetch_or(1 << core, Ordering::Release);
    if core == crate::OVERFLOW_ON_CORE.load(Ordering::SeqCst) {
        crate::kprintln!("[kernel] stack-overflow test on core {core}: recursing");
        crate::kprintln!("[kernel] ... {}", crate::overflow_stack(0));
    }
    sched::set_current(core, usize::MAX);
    let mut tf = BOOT_FRAMES.write(core); // (never released: this core leaves the kernel for good through `enter_user`)
    sched::dispatch(&mut tf, true);
    kick_idle();
    // SAFETY: `tf` points to this core's BOOT_FRAMES slot, just filled by `dispatch`; the guard is never released, so the frame stays
    // valid and unaliased while `enter_user` restores it and erets (it never returns).
    unsafe { enter_user(&*tf) }
}

/// Boot core: enter userspace for the first time.
pub fn boot_core_enter() -> ! {
    let mut tf = BOOT_FRAMES.write(0);
    sched::set_current(0, usize::MAX);
    sched::dispatch(&mut tf, true);
    kick_idle();
    // SAFETY: as in `secondary_main`: `tf` is core 0's BOOT_FRAMES slot, filled by `dispatch`, and held for good.
    unsafe { enter_user(&*tf) }
}
