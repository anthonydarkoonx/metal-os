//! QEMU `virt`: PSCI through the HVC conduit (the machine has no EL3 firmware; the kernel runs at EL1 under QEMU's own handler).
use crate::hal::PowerControl;

pub struct PsciHvc;

impl PowerControl for PsciHvc {
    fn power(reset: bool) {
        let func: u64 = if reset { 0x8400_0009 } else { 0x8400_0008 }; // SYSTEM_RESET / SYSTEM_OFF
        // SMCCC: the callee may change x0..x17 (results and scratch); x18 and up are kept. It may also touch memory (a reset
        // does not return at all), so no `nomem`.
        // SAFETY: PSCI SYSTEM_OFF/RESET through HVC; the clobber list covers x0..x17 (SMCCC), and memory is not excluded, so the compiler
        // assumes the call may touch it. Does not return on success.
        unsafe { core::arch::asm!("hvc #0", inout("x0") func => _, inout("x1") 0u64 => _, inout("x2") 0u64 => _, inout("x3") 0u64 => _, lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _, lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _, lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _, lateout("x16") _, lateout("x17") _, options(nostack)) };
    }

    /// # Safety
    /// As `PowerControl::cpu_on`: `entry` must be the secondary core's entry point, since the core will run there with `core` in x3.
    unsafe fn cpu_on(core: usize, entry: u64) -> i64 {
        let r: i64;
        let target = core as u64; // QEMU: Aff0 = core
        // SAFETY: PSCI CPU_ON through HVC with the contract above; the clobber list covers x0..x17 (SMCCC); the call does not touch Rust-visible memory.
        unsafe { core::arch::asm!("hvc #0", inout("x0") 0xC400_0003u64 => r, inout("x1") target => _, inout("x2") entry => _, inout("x3") core as u64 => _, lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _, lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _, lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _, lateout("x16") _, lateout("x17") _, options(nostack)) };
        r
    }
}
