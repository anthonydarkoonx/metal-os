//! The kernel's *frame*: the safe API over the few things that need `unsafe`.
//!
//! Metal OS follows the framekernel layering (as in Asterinas): code that touches raw memory or CPU
//! registers lives in the *framework* modules (this one, `mmu`, `smmu`, `gic`, `trap`, `smp`, `sync`,
//! `timer`, `cache`, `kaslr`, `boot.S`, device probing), and every other kernel module carries
//! `#![forbid(unsafe_code)]`, so the compiler guarantees it cannot cause memory unsafety by itself.
//!
//! What this module offers:
//!  * `phys`: access to physical RAM by address. Every access is checked to lie wholly inside RAM
//!    (and aligned for the width read), so a stale or forged address makes the kernel panic and
//!    never corrupts memory outside RAM. It does not check *ownership* of the frame: that is the
//!    allocator's and the page tables' job (`mem`, `mmu`).
//!  * `cpu`: the few system registers and instructions the rest of the kernel needs.
use core::sync::atomic::{AtomicUsize, Ordering};

/// Physical access to RAM, bounds-checked.
pub mod phys {
    use super::*;
    use crate::mem::{PAGE, RAM_BASE};
    use core::ptr::{read_volatile, write_volatile};
    use core::sync::atomic::AtomicU32;

    static RAM_END: AtomicUsize = AtomicUsize::new(RAM_BASE);

    /// Record how much RAM there is (once, from `mem::init`).
    pub fn set_ram_size(bytes: usize) {
        RAM_END.store(RAM_BASE + bytes, Ordering::Relaxed);
    }

    fn in_ram(pa: usize, len: usize, align: usize) -> bool {
        pa % align == 0 && pa >= RAM_BASE && pa.checked_add(len).is_some_and(|end| end <= RAM_END.load(Ordering::Relaxed))
    }

    #[inline]
    fn check(pa: usize, len: usize, align: usize) {
        let ok = in_ram(pa, len, align);
        assert!(ok, "frame::phys: access of {len} bytes at {pa:#x} is outside RAM or misaligned");
    }

    pub fn read_u8(pa: usize) -> u8 {
        check(pa, 1, 1);
        unsafe { read_volatile(pa as *const u8) }
    }

    pub fn read_u32(pa: usize) -> u32 {
        check(pa, 4, 4);
        unsafe { read_volatile(pa as *const u32) }
    }

    pub fn read_u64(pa: usize) -> u64 {
        check(pa, 8, 8);
        unsafe { read_volatile(pa as *const u64) }
    }

    pub fn write_u32(pa: usize, v: u32) {
        check(pa, 4, 4);
        unsafe { write_volatile(pa as *mut u32, v) }
    }

    pub fn write_u64(pa: usize, v: u64) {
        check(pa, 8, 8);
        unsafe { write_volatile(pa as *mut u64, v) }
    }

    pub fn write_i64(pa: usize, v: i64) {
        write_u64(pa, v as u64)
    }

    // The `try_` forms are for addresses a *user program* chose (it may have mapped a device page, which is not RAM):
    // they fail instead of panicking.

    pub fn try_read_u8(pa: usize) -> Option<u8> {
        in_ram(pa, 1, 1).then(|| read_u8(pa))
    }

    pub fn try_read_u32(pa: usize) -> Option<u32> {
        in_ram(pa, 4, 4).then(|| read_u32(pa))
    }

    pub fn try_write_u32(pa: usize, v: u32) -> bool {
        in_ram(pa, 4, 4).then(|| write_u32(pa, v)).is_some()
    }

    pub fn try_copy_from(pa: usize, dst: &mut [u8]) -> bool {
        in_ram(pa, dst.len(), 1).then(|| copy_from(pa, dst)).is_some()
    }

    pub fn try_copy_to(pa: usize, src: &[u8]) -> bool {
        in_ram(pa, src.len(), 1).then(|| copy_to(pa, src)).is_some()
    }

    pub fn try_copy_page(dst: usize, src: usize) -> bool {
        (in_ram(dst, PAGE, PAGE) && in_ram(src, PAGE, PAGE)).then(|| copy_page(dst, src)).is_some()
    }

    /// A 32-bit word in RAM shared with user space (the rings), used with atomic orderings.
    pub fn atomic_u32(pa: usize) -> &'static AtomicU32 {
        check(pa, 4, 4);
        unsafe { AtomicU32::from_ptr(pa as *mut u32) }
    }

    /// Copy `src` into RAM at `pa`.
    pub fn copy_to(pa: usize, src: &[u8]) {
        check(pa, src.len(), 1);
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), pa as *mut u8, src.len()) }
    }

    /// Copy RAM at `pa` into `dst`.
    pub fn copy_from(pa: usize, dst: &mut [u8]) {
        check(pa, dst.len(), 1);
        unsafe { core::ptr::copy_nonoverlapping(pa as *const u8, dst.as_mut_ptr(), dst.len()) }
    }

    /// Copy one whole frame to another.
    pub fn copy_page(dst: usize, src: usize) {
        check(dst, PAGE, PAGE);
        check(src, PAGE, PAGE);
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, PAGE) }
    }

    #[allow(dead_code)]
    pub fn zero(pa: usize, len: usize) {
        check(pa, len, 1);
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, len) }
    }

    pub fn zero_page(pa: usize) {
        check(pa, PAGE, PAGE);
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, PAGE) }
    }

    /// Look at the frame at `pa` for the duration of `f`. The caller owns the frame (nobody maps it writable meanwhile).
    pub fn with_page<R>(pa: usize, f: impl FnOnce(&[u8; PAGE]) -> R) -> R {
        check(pa, PAGE, PAGE);
        f(unsafe { &*(pa as *const [u8; PAGE]) })
    }

    /// Modify the frame at `pa` for the duration of `f`. The caller owns the frame.
    pub fn with_page_mut<R>(pa: usize, f: impl FnOnce(&mut [u8; PAGE]) -> R) -> R {
        check(pa, PAGE, PAGE);
        f(unsafe { &mut *(pa as *mut [u8; PAGE]) })
    }
}

/// CPU registers and instructions.
pub mod cpu {
    use core::arch::asm;

    /// The free-running system counter.
    pub fn counter() -> u64 {
        let c: u64;
        unsafe { asm!("mrs {}, cntpct_el0", out(reg) c, options(nomem, nostack)) };
        c
    }

    /// The CPU's identification register.
    pub fn midr() -> u64 {
        let m: u64;
        unsafe { asm!("mrs {}, midr_el1", out(reg) m, options(nomem, nostack)) };
        m
    }

    /// Does the CPU have the hardware random number instruction (RNDR)?
    pub fn has_rndr() -> bool {
        let isar0: u64;
        unsafe { asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
        isar0 >> 60 & 0xF != 0
    }

    /// One word from RNDR (only call when `has_rndr`); `None` if it did not answer this time.
    pub fn rndr() -> Option<u64> {
        let (v, ok): (u64, u64);
        unsafe { asm!("mrs {v}, s3_3_c2_c4_0", "cset {ok}, ne", v = out(reg) v, ok = out(reg) ok, options(nomem, nostack)) };
        (ok != 0).then_some(v)
    }

    /// Ask the firmware (PSCI: the platform knows through which conduit) to switch the machine off or restart it. Returns only if the firmware refused.
    pub fn psci_power(reset: bool) {
        crate::platform::psci_power(reset);
    }

    /// Sleep until an event or interrupt.
    pub fn wait_for_event() {
        unsafe { asm!("wfe") };
    }

    /// End the run with `code` where the machine can (the platform: an emulator's semihosting); returns if it cannot.
    pub fn semihost_exit(code: u32) {
        crate::platform::exit_machine(code);
    }
}
