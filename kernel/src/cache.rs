//! Data-cache maintenance by address, for memory a device shares with the kernel (the SMMU's stream table,
//! translation tables and queues). Needed when the device does not snoop the CPU caches; harmless when it does.
//! (Userspace drivers do the same for their buffers: `user/src/dma.rs` has the rules.)
use core::arch::asm;

/// The smallest data cache line, from CTR_EL0.DminLine.
pub fn line() -> usize {
    let ctr: u64;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    4usize << ((ctr >> 16) & 0xF)
}

/// Write back `addr..addr+len` so a device that reads memory sees what the CPU wrote.
pub fn clean(addr: usize, len: usize) {
    let step = line();
    let mut a = addr & !(step - 1);
    while a < addr + len {
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += step;
    }
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Drop `addr..addr+len` from the caches so the next read fetches what a device wrote. The range must not share a
/// cache line with data the CPU has written and not cleaned (here: whole queue entries, 16 or 32 bytes inside a
/// page that holds nothing but the queue).
pub fn invalidate(addr: usize, len: usize) {
    // The caller learned the device had written (a status or index load); that load must complete before the lines are
    // dropped, or the invalidate could run ahead of it and the later read see the old data.
    unsafe { asm!("dsb ld", options(nostack)) };
    let step = line();
    let mut a = addr & !(step - 1);
    while a < addr + len {
        unsafe { asm!("dc ivac, {}", in(reg) a, options(nostack)) };
        a += step;
    }
    unsafe { asm!("dsb sy", options(nostack)) };
}
