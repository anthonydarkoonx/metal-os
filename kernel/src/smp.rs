//! Symmetric multiprocessing: core bring-up through PSCI and idle-core wakeups by IPI.
//!
//! There is no global kernel lock: kernel code on different cores runs in parallel and
//! synchronises through the fine-grained locks described in `sync.rs`. A core with nothing
//! to run sleeps in `wfi` with no lock held; whoever makes work runnable sends it an IPI, and a busy
//! core that runs something less important than a task that just became runnable gets one too.
use crate::sync::Global;
use crate::trap::TrapFrame;
use crate::{gic, kprintln, mmu, sched, timer};
use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

pub const MAX_CPUS: usize = 4;
const SGI_WAKE: u32 = 0;

/// Cores sleeping in `idle()`.
static IDLE_MASK: AtomicU32 = AtomicU32::new(0);
/// Cores that have completed bring-up (bit 0 = boot core).
static ONLINE: AtomicU32 = AtomicU32::new(1);

/// This core's number. Kept in `tpidr_el1` (set once at bring-up): reading it is a plain register
/// read, while `mpidr_el1` is a call into the emulator, and this is asked on every scheduling step.
pub fn cpu_id() -> usize {
    let m: u64;
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
    unsafe {
        // wfi wakes on a *pending* interrupt even while masked, so an IPI sent just before
        // this point cannot be lost. Then unmask just long enough to take it.
        asm!("wfi", "msr daifclr, #2", "isb", "msr daifset, #2", options(nomem, nostack));
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

static BOOT_FRAMES: Global<[TrapFrame; MAX_CPUS]> = Global::new([TrapFrame::ZERO; MAX_CPUS]);

/// PSCI CPU_ON (the platform knows the conduit and how a core is numbered): the core starts at `_secondary_start`, its number as the context id.
fn psci_cpu_on(core: usize) -> i64 {
    // SAFETY: `_secondary_start` is the secondary cores' entry point.
    unsafe { crate::platform::psci_cpu_on(core, _secondary_start as *const () as usize as u64) }
}

/// Boot core: power up whatever other cores exist.
pub fn start_secondaries() -> usize {
    let mut started = 0;
    for core in 1..MAX_CPUS {
        match psci_cpu_on(core) {
            0 => started += 1,
            -2 => break, // INVALID_PARAMETERS: no such core
            e => {
                kprintln!("[smp] PSCI CPU_ON({core}) failed: {e}");
                break;
            }
        }
    }
    started
}

/// Entry for each secondary core, called from boot.S with the MMU off.
#[no_mangle]
extern "C" fn secondary_main(core: usize) -> ! {
    unsafe { asm!("msr tpidr_el1, {}", in(reg) core as u64, options(nomem, nostack)) };
    mmu::enable_this_cpu();
    timer::init_core();
    ONLINE.fetch_or(1 << core, Ordering::Release);
    if core == 1 && crate::OVERFLOW_ON_CORE1.load(Ordering::SeqCst) {
        crate::kprintln!("[kernel] stack-overflow test on core 1: recursing");
        crate::kprintln!("[kernel] ... {}", crate::overflow_stack(0));
    }
    sched::set_current(core, usize::MAX);
    let tf = &mut BOOT_FRAMES.get()[core];
    sched::dispatch(tf, true);
    kick_idle();
    unsafe { enter_user(tf) }
}

/// Boot core: enter userspace for the first time.
pub fn boot_core_enter() -> ! {
    let tf = &mut BOOT_FRAMES.get()[0];
    sched::set_current(0, usize::MAX);
    sched::dispatch(tf, true);
    kick_idle();
    unsafe { enter_user(tf) }
}
