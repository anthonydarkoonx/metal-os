//! Cache maintenance for DMA.
//!
//! A device reads and writes memory, not the CPU's caches. On a machine where the device is not cache coherent
//! (most real SoCs) the CPU and the device see different data unless
//! software cleans (writes back) what the device is to read and discards (invalidates) what it has written.
//! QEMU's virtio devices are coherent, so nothing here changes what QEMU does; it is what the same drivers need
//! on real hardware, and it is cheap.
//!
//! The rules the drivers follow, for every buffer of a transfer:
//! * the device **reads** it (descriptors, rings the device reads, request headers, data to write): `clean` after the
//!   CPU's last write and before the device is told;
//! * the device **writes** it (data read from a disk, status bytes, the used ring, input events): `flush` before the
//!   device is told (so no dirty line of it can be evicted over the device's data later) and again after the device
//!   reports completion (so no stale copy, fetched speculatively in between, is read instead of the device's data).
//!
//! Only `DC CVAC` (clean) and `DC CIVAC` (clean and invalidate) are used, because those are the maintenance operations
//! an EL0 driver may execute (the kernel sets `SCTLR_EL1.UCI`); invalidation alone (`DC IVAC`) is EL1-only. Clean and
//! invalidate is also the safe form: a partially covered line is written back before it is dropped, so a neighbour's
//! data in the same line is never lost.
use core::sync::atomic::{AtomicUsize, Ordering};

// The four instructions this file needs. On the machine they are AArch64's; on the host (where this crate is only compiled and its pure parts tested)
// they do nothing.
#[cfg(target_arch = "aarch64")]
mod hw {
    use core::arch::asm;
    pub fn ctr_el0() -> u64 {
        let ctr: u64;
        // SAFETY: reading CTR_EL0 is allowed at EL0 (kernel sets SCTLR_EL1.UCT); the asm touches only the output register.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
        ctr
    }
    pub fn clean_line(a: usize) {
        // SAFETY: DC CVAC is EL0-legal (SCTLR_EL1.UCI) and only writes a line back, changing no value; callers pass mapped task addresses (clean() skips 0).
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack)) };
    }
    pub fn flush_line(a: usize) {
        // SAFETY: DC CIVAC is EL0-legal (SCTLR_EL1.UCI) and writes back before dropping, so no data is lost; callers pass mapped task addresses (flush() skips 0).
        unsafe { asm!("dc civac, {}", in(reg) a, options(nostack)) };
    }
    pub fn barrier() {
        // SAFETY: DSB SY is a pure barrier with no operands, memory effects or register clobbers.
        unsafe { asm!("dsb sy", options(nostack)) };
    }
}
#[cfg(not(target_arch = "aarch64"))]
mod hw {
    pub fn ctr_el0() -> u64 {
        4 << 16 // 64-byte lines
    }
    pub fn clean_line(_a: usize) {}
    pub fn flush_line(_a: usize) {}
    pub fn barrier() {}
}

static LINE: AtomicUsize = AtomicUsize::new(0);

/// The smallest data cache line, from CTR_EL0.DminLine (EL0 may read CTR_EL0: the kernel sets `SCTLR_EL1.UCT`).
fn line() -> usize {
    let l = LINE.load(Ordering::Relaxed);
    if l != 0 {
        return l;
    }
    let ctr = hw::ctr_el0();
    let l = 4usize << ((ctr >> 16) & 0xF);
    LINE.store(l, Ordering::Relaxed);
    l
}

/// Write back every line of `va..va+len` to memory (so a device reading it sees what the CPU wrote).
pub fn clean(va: usize, len: usize) {
    if len == 0 || va == 0 {
        return; // (address 0 is never mapped: real cores fault on cache maintenance of an unmapped address, QEMU does not)
    }
    let step = line();
    let mut a = va & !(step - 1);
    while a < va + len {
        hw::clean_line(a);
        a += step;
    }
    hw::barrier();
}

/// Write back and drop every line of `va..va+len` (so the next CPU read fetches what a device wrote, and no
/// dirty line can overwrite it).
pub fn flush(va: usize, len: usize) {
    if len == 0 || va == 0 {
        return; // (see `clean`)
    }
    let step = line();
    let mut a = va & !(step - 1);
    while a < va + len {
        hw::flush_line(a);
        a += step;
    }
    hw::barrier();
}

/// Self-test for the conductor: the line size is readable from EL0 and sane, and maintenance over an odd-sized,
/// unaligned range leaves the data (including the bytes next to it in the same lines) untouched.
pub fn selftest() -> (usize, bool) {
    extern crate alloc;
    let mut v = alloc::vec![0u8; 4096 + 256];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i * 7 + 1) as u8;
    }
    let want = v.clone();
    let base = v.as_ptr() as usize;
    clean(base + 3, 1000);
    flush(base + 131, 2000);
    clean(base, 0);
    flush(0, 64); // a buffer that is not mapped here: skipped
    (line(), v == want && line().is_power_of_two() && (16..=256).contains(&line()))
}
