//! Data-cache maintenance by address, for memory a device shares with the kernel (the SMMU's stream table,
//! translation tables and queues). Needed when the device does not snoop the CPU caches; harmless when it does.
//! (Userspace drivers do the same for their buffers: `user/src/dma.rs` has the rules.)
use core::arch::asm;

/// The smallest data cache line, from CTR_EL0.DminLine.
pub fn line() -> usize {
    let ctr: u64;
    // SAFETY: reads CTR_EL0, a read-only register; no memory is accessed.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    4usize << ((ctr >> 16) & 0xF)
}

/// Write back `addr..addr+len` so a device that reads memory sees what the CPU wrote.
pub fn clean(addr: usize, len: usize) {
    let step = line();
    let addr = crate::frame::phys::cache_va(addr);
    let mut a = addr & !(step - 1);
    while a < addr + len {
        // SAFETY: `dc cvac` on one line inside the caller's `addr..addr+len`; a clean only writes back, it never alters data.
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += step;
    }
    // SAFETY: barrier only; completes the cleans above before the caller tells the device.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Drop `addr..addr+len` from the caches so the next read fetches what a device wrote. The range must not share a
/// cache line with data the CPU has written and not cleaned (here: whole queue entries, 16 or 32 bytes inside a
/// page that holds nothing but the queue).
pub fn invalidate(addr: usize, len: usize) {
    // The caller learned the device had written (a status or index load); that load must complete before the lines are
    // dropped, or the invalidate could run ahead of it and the later read see the old data.
    // SAFETY: barrier only; orders the caller's device-written status load before the invalidates below.
    unsafe { asm!("dsb ld", options(nostack)) };
    let step = line();
    let addr = crate::frame::phys::cache_va(addr);
    let mut a = addr & !(step - 1);
    while a < addr + len {
        // SAFETY: `dc ivac` on a line inside the caller's range; per the doc contract the range is whole queue entries in a page that
        // holds nothing else, so no un-cleaned CPU write shares the line and is lost.
        unsafe { asm!("dc ivac, {}", in(reg) a, options(nostack)) };
        a += step;
    }
    // SAFETY: barrier only; completes the invalidates above.
    unsafe { asm!("dsb sy", options(nostack)) };
}
