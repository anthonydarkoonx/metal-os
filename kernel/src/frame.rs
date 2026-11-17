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

    /// Is `pa` the start of a whole, aligned frame of RAM?
    pub fn is_ram_page(pa: usize) -> bool {
        in_ram(pa, PAGE, PAGE)
    }

    #[inline]
    fn check(pa: usize, len: usize, align: usize) {
        let ok = in_ram(pa, len, align);
        assert!(ok, "frame::phys: access of {len} bytes at {pa:#x} is outside RAM or misaligned");
    }

    pub fn read_u8(pa: usize) -> u8 {
        check(pa, 1, 1);
        // SAFETY: `check` above made `pa..pa+1` lie inside RAM, aligned for the width; RAM is identity-mapped normal memory. Volatile
        // because a user program or device may share the frame; no reference to it is created.
        unsafe { read_volatile(pa as *const u8) }
    }

    pub fn read_u32(pa: usize) -> u32 {
        check(pa, 4, 4);
        // SAFETY: `check` above made `pa..pa+4` lie inside RAM, aligned for the width; RAM is identity-mapped normal memory. Volatile
        // because a user program or device may share the frame; no reference to it is created.
        unsafe { read_volatile(pa as *const u32) }
    }

    pub fn read_u64(pa: usize) -> u64 {
        check(pa, 8, 8);
        // SAFETY: `check` above made `pa..pa+8` lie inside RAM, aligned for the width; RAM is identity-mapped normal memory. Volatile
        // because a user program or device may share the frame; no reference to it is created.
        unsafe { read_volatile(pa as *const u64) }
    }

    pub fn write_u32(pa: usize, v: u32) {
        check(pa, 4, 4);
        // SAFETY: `check` above made `pa..pa+4` lie inside RAM, aligned for the width; RAM is identity-mapped normal memory. Volatile
        // because a user program or device may share the frame; no reference to it is created.
        unsafe { write_volatile(pa as *mut u32, v) }
    }

    pub fn write_u64(pa: usize, v: u64) {
        check(pa, 8, 8);
        // SAFETY: `check` above made `pa..pa+8` lie inside RAM, aligned for the width; RAM is identity-mapped normal memory. Volatile
        // because a user program or device may share the frame; no reference to it is created.
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
        // SAFETY: `check` made the word in RAM and 4-aligned (valid for `AtomicU32`), and RAM stays mapped for the kernel's life, so
        // `'static` holds. Mixing these with non-atomic `copy_*` on the same word would be a race; callers use it for ring words only.
        unsafe { AtomicU32::from_ptr(pa as *mut u32) }
    }

    /// Copy `src` into RAM at `pa`.
    pub fn copy_to(pa: usize, src: &[u8]) {
        check(pa, src.len(), 1);
        // SAFETY: `check` covered `pa..pa+len` in RAM; `src` is a live slice and `ptr::copy` allows overlap; raw pointers only.
        unsafe { core::ptr::copy(src.as_ptr(), pa as *mut u8, src.len()) }
    }

    /// Copy RAM at `pa` into `dst`.
    pub fn copy_from(pa: usize, dst: &mut [u8]) {
        check(pa, dst.len(), 1);
        // SAFETY: `check` covered `pa..pa+len` in RAM; `dst` is a live exclusive slice and `ptr::copy` allows overlap; raw pointers only.
        unsafe { core::ptr::copy(pa as *const u8, dst.as_mut_ptr(), dst.len()) }
    }

    /// Copy one whole frame to another.
    pub fn copy_page(dst: usize, src: usize) {
        check(dst, PAGE, PAGE);
        check(src, PAGE, PAGE);
        // SAFETY: both frames were `check`ed whole, page-aligned and inside RAM; `ptr::copy` is correct even if they were the same frame.
        unsafe { core::ptr::copy(src as *const u8, dst as *mut u8, PAGE) }
    }

    #[allow(dead_code)]
    pub fn zero(pa: usize, len: usize) {
        check(pa, len, 1);
        // SAFETY: `check` covered `pa..pa+len` in RAM; byte writes need no alignment; no reference to the range exists here.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, len) }
    }

    pub fn zero_page(pa: usize) {
        check(pa, PAGE, PAGE);
        // SAFETY: `check` covered the whole frame in RAM; byte writes, no reference to the frame is created.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, PAGE) }
    }

    // Frames lent out as `&` / `&mut` by `with_page` / `with_page_mut`. Lending a frame that is already lent (the same page
    // reached twice, whether by the same core or two cores) would give out aliasing references, so it panics instead.
    const LENT_SLOTS: usize = 16;
    static LENT: [AtomicUsize; LENT_SLOTS] = [const { AtomicUsize::new(0) }; LENT_SLOTS];

    struct Lent(usize);

    impl Lent {
        fn new(pa: usize) -> Lent {
            // (RAM_BASE is not 0, so 0 can mean "free")
            let Some(mine) = (0..LENT_SLOTS).find(|&i| LENT[i].compare_exchange(0, pa, Ordering::AcqRel, Ordering::Relaxed).is_ok()) else {
                panic!("frame::phys: too many frames lent at once");
            };
            // look for another entry for the same frame, after ours is published (two cores lending it at once see each other)
            if (0..LENT_SLOTS).any(|i| i != mine && LENT[i].load(Ordering::Acquire) == pa) {
                LENT[mine].store(0, Ordering::Release);
                panic!("frame::phys: frame {pa:#x} lent twice (aliasing references)");
            }
            Lent(mine)
        }
    }

    impl Drop for Lent {
        fn drop(&mut self) {
            LENT[self.0].store(0, Ordering::Release);
        }
    }

    /// Look at the frame at `pa` for the duration of `f`. The caller owns the frame (nobody maps it writable meanwhile);
    /// a second loan of the same frame while this one lives panics.
    pub fn with_page<R>(pa: usize, f: impl FnOnce(&[u8; PAGE]) -> R) -> R {
        check(pa, PAGE, PAGE);
        let _loan = Lent::new(pa);
        // SAFETY: the page is inside RAM and aligned (checked); the loan table guarantees no other reference from here exists
        // for the duration; the caller's contract (it owns the frame: no mapping, no device) covers everything that is not a
        // reference.
        f(unsafe { &*(pa as *const [u8; PAGE]) })
    }

    /// Modify the frame at `pa` for the duration of `f`. The caller owns the frame; a second loan of it while this one
    /// lives panics.
    pub fn with_page_mut<R>(pa: usize, f: impl FnOnce(&mut [u8; PAGE]) -> R) -> R {
        check(pa, PAGE, PAGE);
        let _loan = Lent::new(pa);
        // SAFETY: as in `with_page`.
        f(unsafe { &mut *(pa as *mut [u8; PAGE]) })
    }
}

/// CPU registers and instructions.
pub mod cpu {
    use core::arch::asm;

    /// The free-running system counter.
    pub fn counter() -> u64 {
        let c: u64;
        // SAFETY: reads CNTPCT_EL0 (EL1 may always read it); no memory is accessed.
        unsafe { asm!("mrs {}, cntpct_el0", out(reg) c, options(nomem, nostack)) };
        c
    }

    /// The CPU's identification register.
    pub fn midr() -> u64 {
        let m: u64;
        // SAFETY: reads MIDR_EL1, a read-only identification register; no memory is accessed.
        unsafe { asm!("mrs {}, midr_el1", out(reg) m, options(nomem, nostack)) };
        m
    }

    /// Does the CPU have the hardware random number instruction (RNDR)?
    pub fn has_rndr() -> bool {
        let isar0: u64;
        // SAFETY: reads ID_AA64ISAR0_EL1, a read-only feature register; no memory is accessed.
        unsafe { asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
        isar0 >> 60 & 0xF != 0
    }

    /// One word from RNDR (only call when `has_rndr`); `None` if it did not answer this time.
    pub fn rndr() -> Option<u64> {
        let (v, ok): (u64, u64);
        // SAFETY: RNDR is read only after `has_rndr` said it exists (every caller checks; `rand::rndr` via HAS_RNDR); the read has no
        // memory effect, and a failed read is reported through the Z flag (`ok`), not by trapping.
        unsafe { asm!("mrs {v}, s3_3_c2_c4_0", "cset {ok}, ne", v = out(reg) v, ok = out(reg) ok, options(nomem, nostack)) };
        (ok != 0).then_some(v)
    }

    /// Ask the firmware (PSCI: the platform knows through which conduit) to switch the machine off or restart it. Returns only if the firmware refused.
    pub fn psci_power(reset: bool) {
        crate::platform::psci_power(reset);
    }

    /// Sleep until an event or interrupt.
    pub fn wait_for_event() {
        // SAFETY: WFE only pauses the core until an event or interrupt; it changes no state the compiler tracks.
        unsafe { asm!("wfe") };
    }

    /// End the run with `code` where the machine can (the platform: an emulator's semihosting); returns if it cannot.
    pub fn semihost_exit(code: u32) {
        crate::platform::exit_machine(code);
    }
}
