//! QEMU `virt`: PSCI through the HVC conduit (the machine has no EL3 firmware; the kernel runs at EL1 under QEMU's own handler).
use crate::hal::PowerControl;

pub struct PsciHvc;

impl PowerControl for PsciHvc {
    fn power(reset: bool) {
        let func: u64 = if reset { 0x8400_0009 } else { 0x8400_0008 }; // SYSTEM_RESET / SYSTEM_OFF
        unsafe { core::arch::asm!("hvc #0", inout("x0") func => _, options(nomem, nostack)) };
    }

    unsafe fn cpu_on(core: usize, entry: u64) -> i64 {
        let r: i64;
        let target = core as u64; // QEMU: Aff0 = core
        core::arch::asm!("hvc #0", inout("x0") 0xC400_0003u64 => r, inout("x1") target => _, inout("x2") entry => _, inout("x3") core as u64 => _, options(nostack));
        r
    }
}
