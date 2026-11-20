//! Kernel address randomisation: where in RAM the kernel image lives.
//!
//! RAM is identity-mapped and the kernel addresses frames by physical address everywhere, so the kernel cannot move in
//! the virtual address space on its own; what it can do is sit at a different physical (and therefore virtual) address
//! on each boot. QEMU loads the image at its link address, 0x4008_0000. `boot.S` applies the image's relocations, calls
//! `kaslr_place` here (MMU off: no locks, no atomics, only plain accesses) and, if it names an address, copies the
//! image there, corrects the copy's pointers and continues in the copy. The old image's memory is cleared and given
//! back to the allocator by `kmain` (`take_old_image`).
//!
//! The address is any page in RAM where the whole image (code, data, .bss and the stacks) fits without touching the
//! first 512 KiB (QEMU keeps the device tree there) or the old image. With 64 MiB of RAM that is about 14 bits of
//! choice. The generator is seeded like the one for the program layouts (`rand.rs`): RNDR, the first bytes of the
//! harness's seed file, the counter. `opt/metal/noaslr` does not fix this address: it is a debugging aid for the
//! layouts of programs, and a kernel that can be told to sit still gives that up too easily.
use crate::mem::{PAGE, RAM_BASE};
use core::arch::asm;
use core::ptr::read_volatile;
use core::sync::atomic::{AtomicU64, Ordering};
use metal_rand::Rng;

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static kaslr_old_base: u64;
}

/// The address the image was linked (and loaded) at.
pub const LINK_BASE: usize = crate::platform::LINK_BASE;

/// Why the image stayed or moved; set before the image is copied, so these must live in .data (not .bss: the copy's .bss is zeroed).
pub const MOVED: u64 = 0;
pub const NO_RAM_SIZE: u64 = 1;
pub const NO_ROOM: u64 = 2;
pub const NOT_RUN: u64 = 3;
static OUTCOME: AtomicU64 = AtomicU64::new(NOT_RUN);
/// 1 if the generator had neither RNDR nor the seed file (only the counter): the address is guessable. (Starts as 2, not
/// 0, so that it lives in .data and survives the move: the copy's .bss starts out zeroed.)
static WEAK: AtomicU64 = AtomicU64::new(2);

fn counter() -> u64 {
    let c: u64;
    // SAFETY: reads CNTPCT_EL0, always readable at EL1; no memory is accessed.
    unsafe { asm!("mrs {}, cntpct_el0", out(reg) c, options(nomem, nostack)) };
    c
}

fn rndr() -> Option<u64> {
    let isar0: u64;
    // SAFETY: reads ID_AA64ISAR0_EL1, a read-only feature register; no memory is accessed.
    unsafe { asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
    if isar0 >> 60 & 0xF == 0 {
        return None;
    }
    for _ in 0..4 {
        let (v, ok): (u64, u64);
        // SAFETY: reached only when ISAR0.RNDR != 0 (checked above), so the RNDR register exists; no memory is accessed.
        unsafe { asm!("mrs {v}, s3_3_c2_c4_0", "cset {ok}, ne", v = out(reg) v, ok = out(reg) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

/// Called from `boot.S` with the MMU off. The address to move the image to, or 0 to leave it where it is.
#[no_mangle]
extern "C" fn kaslr_place(dtb: usize) -> usize {
    crate::platform::set_dtb(dtb);
    // SAFETY: `__kernel_start` is a linker symbol; only its address is taken, it is never read.
    let start = unsafe { &__kernel_start as *const u8 as usize };
    // SAFETY: `__kernel_end` is a linker symbol; only its address is taken, it is never read.
    let size = (unsafe { &__kernel_end as *const u8 as usize } - start + PAGE - 1) & !(PAGE - 1);
    let Some(ram) = crate::platform::ram_size().filter(|&s| (8 << 20..=crate::frame::phys::MAX_RAM as u64).contains(&s)) else {
        OUTCOME.store(NO_RAM_SIZE, Ordering::Relaxed);
        return 0;
    };
    // only the identity-mapped window (the first GiB), and clear of the frame tables the allocator puts in the last frames of RAM
    let ram = crate::mem::usable_ram(ram as usize);
    let tables = if ram <= crate::frame::phys::GIB { crate::mem::meta_bytes(ram / PAGE) } else { 0 };
    let top = RAM_BASE + ram.min(crate::frame::phys::ID_END - RAM_BASE) - tables;
    if top < LINK_BASE + size {
        OUTCOME.store(NO_ROOM, Ordering::Relaxed);
        return 0;
    }
    let mut seed = [0u8; 32];
    let from_file = crate::platform::bytes(b"opt/metal/seed", &mut seed);
    let mut rng = Rng::from_seed(seed);
    let mut hw = [0u8; 32];
    let mut rndrs = 0;
    for c in hw.chunks_mut(8) {
        if let Some(v) = rndr() {
            c.copy_from_slice(&v.to_le_bytes());
            rndrs += 1;
        }
    }
    rng.mix(&hw);
    rng.mix(&counter().to_le_bytes());
    rng.mix(b"metal-os kaslr");
    WEAK.store((from_file != 32 && rndrs != 4) as u64, Ordering::Relaxed);
    // candidates: page-aligned addresses from LINK_BASE up, where the image fits below the top of RAM
    let n = ((top - size - LINK_BASE) / PAGE + 1) as u64;
    for _ in 0..64 {
        let at = LINK_BASE + rng.below(n) as usize * PAGE;
        // (clear of the old image, which is still being read, and of the device tree, which is read after the move)
        let clear_of = |lo: usize, len: usize| at + size <= lo || at >= lo + len;
        if clear_of(start, size) && crate::platform::dtb_span().is_none_or(|(d, n)| clear_of(d, n)) {
            OUTCOME.store(MOVED, Ordering::Relaxed);
            return at;
        }
    }
    OUTCOME.store(NO_ROOM, Ordering::Relaxed);
    0
}

/// The old image's address range if the image moved (it is still there, to be cleared and freed), and what happened.
pub struct Report {
    pub outcome: u64,
    pub weak: bool,
    pub old: Option<(usize, usize)>,
}

pub fn report() -> Report {
    // SAFETY: `kaslr_old_base` is a u64 defined in boot.S (.data, set before the copy); nothing writes it after boot, so the read is
    // aligned, initialised and race-free.
    let old = unsafe { read_volatile(&kaslr_old_base) } as usize;
    // SAFETY: both are linker symbols; only their addresses are taken, they are never read.
    let size = unsafe { &__kernel_end as *const u8 as usize - &__kernel_start as *const u8 as usize };
    Report { outcome: OUTCOME.load(Ordering::Relaxed), weak: WEAK.load(Ordering::Relaxed) == 1, old: (old != 0).then_some((old, size)) }
}
