//! PSCI: the firmware's power and core start-up calls, through the conduit the device tree names (`/psci` `method`): HVC on QEMU `virt` (the machine has
//! no EL3 firmware; the kernel runs at EL1 under QEMU's own handler), SMC on most boards.
use crate::hal::PowerControl;
use core::sync::atomic::Ordering;

pub struct Psci;

/// SMCCC call: the callee may change x0..x17 (results and scratch); x18 and up are kept. It may also touch memory (a reset does not return at all), so no `nomem`.
fn call(func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: i64;
    if super::CONDUIT_SMC.load(Ordering::Relaxed) != 0 {
        // SAFETY: a PSCI call through SMC; the clobber list covers x0..x17 (SMCCC), and memory is not excluded, so the compiler assumes the call may touch it.
        unsafe { core::arch::asm!("smc #0", inout("x0") func => r, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _, lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _, lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _, lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _, lateout("x16") _, lateout("x17") _, options(nostack)) };
    } else {
        // SAFETY: as above, through HVC.
        unsafe { core::arch::asm!("hvc #0", inout("x0") func => r, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _, lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _, lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _, lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _, lateout("x16") _, lateout("x17") _, options(nostack)) };
    }
    r
}

impl Psci {
    /// Is there a core `core`? (PSCI AFFINITY_INFO answers INVALID_PARAMETERS, -2, for a core that does not exist.)
    pub fn core_exists(core: usize) -> bool {
        call(0xC400_0004, crate::platform::cpu_mpidr(core), 0, 0) >= 0
    }
}

impl PowerControl for Psci {
    fn power(reset: bool) {
        call(if reset { 0x8400_0009 } else { 0x8400_0008 }, 0, 0, 0); // SYSTEM_RESET / SYSTEM_OFF
    }

    /// # Safety
    /// As `PowerControl::cpu_on`: `entry` must be the secondary core's entry point, since the core will run there with `context` in x0.
    unsafe fn cpu_on(core: usize, entry: u64, context: u64) -> i64 {
        // the target is the core's MPIDR affinity (the tree's `reg`); the context id is what `_secondary_start` finds in x0 (its stack and its number)
        call(0xC400_0003, crate::platform::cpu_mpidr(core), entry, context)
    }
}
