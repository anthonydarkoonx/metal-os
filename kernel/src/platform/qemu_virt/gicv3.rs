//! QEMU `virt` with `gic-version=3` (or any GICv3/v4 machine whose device tree says so): the distributor, one redistributor per core, and the CPU
//! interface through system registers (`ICC_*_EL1`). Affinity routing is on, so a core is named by its MPIDR affinity, not by a bit in a mask.
use crate::hal::IrqController;
use crate::smp::{cpu_id, MAX_CPUS};
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Distributor and the start of the redistributor region (from the device tree; the QEMU `virt` addresses until it has been read).
pub static GICD: AtomicUsize = AtomicUsize::new(0x0800_0000);
pub static GICR: AtomicUsize = AtomicUsize::new(0x080A_0000);
/// Size of the redistributor region: where the search for a core's frame stops if no frame carries the `Last` bit.
pub static GICR_SIZE: AtomicUsize = AtomicUsize::new(0x20000 * MAX_CPUS);
/// Each core's own redistributor frame (the 64 KiB `RD_base`; the SGI/PPI frame is 64 KiB above it), set by that core in `init_cpu`.
static RD: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

const FRAME: usize = 0x20000; // RD_base + SGI_base (GICv3; GICv4 has two more 64 KiB frames, and `GICR_TYPER` says so)
const SGI_BASE: usize = 0x10000;

fn r32(addr: usize) -> u32 {
    // SAFETY: callers pass a GICD/GICR register (4-aligned) inside the device block `map_devices` maps; volatile MMIO read.
    unsafe { read_volatile(addr as *const u32) }
}
fn w32(addr: usize, v: u32) {
    // SAFETY: as `r32`, for a write.
    unsafe { write_volatile(addr as *mut u32, v) };
}
fn w64(addr: usize, v: u64) {
    // SAFETY: callers pass an 8-aligned 64-bit GICD register (`GICD_IROUTER`) inside the mapped device block; volatile MMIO write.
    unsafe { write_volatile(addr as *mut u64, v) };
}
fn w8(addr: usize, v: u8) {
    // SAFETY: callers pass a byte-wide priority register inside the mapped device block; volatile MMIO write.
    unsafe { write_volatile(addr as *mut u8, v) };
}
fn gicd() -> usize {
    GICD.load(Ordering::Relaxed)
}

/// MPIDR as the redistributor and the router write it: Aff3 Aff2 Aff1 Aff0 in 32 bits.
fn affinity(mpidr: u64) -> u32 {
    ((mpidr >> 32 & 0xFF) << 24 | (mpidr & 0xFF_FFFF)) as u32
}

/// This core's redistributor frame, found by its affinity.
fn find_redistributor(aff: u32) -> Option<usize> {
    let (base, size) = (GICR.load(Ordering::Relaxed), GICR_SIZE.load(Ordering::Relaxed));
    let mut at = base;
    while at + FRAME <= base + size {
        let typer = (r32(at + 0x8) as u64) | (r32(at + 0xC) as u64) << 32; // GICR_TYPER
        if (typer >> 32) as u32 == aff {
            return Some(at);
        }
        if typer & 1 << 4 != 0 {
            return None; // Last
        }
        // GICv4 frames are 0x40000: GICR_TYPER.VLPIS (bit 1) says so
        at += if typer & 2 != 0 { 2 * FRAME } else { FRAME };
    }
    None
}

fn wait_rwp(ctlr: usize) {
    for _ in 0..1_000_000 {
        if r32(ctlr) & 1 << 31 == 0 {
            return;
        }
    }
}

fn rd() -> usize {
    RD[cpu_id()].load(Ordering::Relaxed)
}

pub struct GicV3;

impl IrqController for GicV3 {
    fn init_dist() {
        let d = gicd();
        w32(d, 0); // GICD_CTLR: off while it is set up
        wait_rwp(d);
        let lines = 32 * ((r32(d + 0x4) & 0x1F) as usize + 1); // GICD_TYPER.ITLinesNumber
        for n in 1..lines / 32 {
            w32(d + 0x080 + 4 * n, !0); // GICD_IGROUPR: every shared interrupt is Group 1 (what ICC_IAR1 acknowledges)
        }
        w32(d, 1 << 4 | 1 << 1 | 1); // ARE, EnableGrp1(NS), EnableGrp0
        wait_rwp(d);
    }

    fn init_cpu() {
        let core = cpu_id();
        let mpidr: u64;
        // SAFETY: reads MPIDR_EL1, a read-only register; no memory is accessed.
        unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
        let Some(rd) = find_redistributor(affinity(mpidr)) else {
            crate::kprintln!("[gic] no redistributor for core {core} (MPIDR {mpidr:#x})");
            return;
        };
        RD[core].store(rd, Ordering::Relaxed);
        // wake the redistributor
        let waker = r32(rd + 0x14);
        w32(rd + 0x14, waker & !(1 << 1)); // GICR_WAKER.ProcessorSleep = 0
        for _ in 0..1_000_000 {
            if r32(rd + 0x14) & 1 << 2 == 0 {
                break; // ChildrenAsleep = 0
            }
        }
        let sgi = rd + SGI_BASE;
        w32(sgi + 0x080, !0); // GICR_IGROUPR0: SGIs and PPIs are Group 1
        for i in 0..32 {
            w8(sgi + 0x400 + i, 0x20); // GICR_IPRIORITYR: the inter-processor interrupts and the timer outrank device lines (0x80, 0x40)
        }
        w32(sgi + 0x100, 0xFFFF | 1 << 30); // GICR_ISENABLER0: SGIs 0..15 + the timer PPI
        // the CPU interface
        // SAFETY: system-register setup of this core's own GIC CPU interface (ICC_SRE_EL1, ICC_PMR_EL1, ICC_IGRPEN1_EL1); no memory is accessed.
        // The boot code let EL1 use the system registers (ICC_SRE_EL2), on a machine whose firmware leaves EL2 to the kernel.
        unsafe {
            asm!(
                "mrs {t}, S3_0_C12_C12_5", // ICC_SRE_EL1
                "orr {t}, {t}, #7",         // SRE | DFB | DIB
                "msr S3_0_C12_C12_5, {t}",
                "isb",
                "msr S3_0_C4_C6_0, {ff}",   // ICC_PMR_EL1: every priority
                "msr S3_0_C12_C12_7, {one}", // ICC_IGRPEN1_EL1
                "isb",
                t = out(reg) _, ff = in(reg) 0xFFu64, one = in(reg) 1u64,
                options(nomem, nostack)
            )
        };
    }

    fn send_sgi_to(mask: u32, id: u32) {
        // SAFETY: barrier only; orders the sender's earlier stores before the SGI write.
        unsafe { asm!("dsb ishst", options(nostack)) };
        let mut left = mask;
        while left != 0 {
            let first = left.trailing_zeros() as usize;
            let t = crate::platform::cpu_mpidr(first);
            // one write per (Aff3, Aff2, Aff1, range of 16 Aff0): the cores of `left` that share them go in its target list
            let group = |m: u64| (m >> 8 & 0xFFFF, m >> 32 & 0xFF, m & 0xF0);
            let mut list = 0u64;
            for c in 0..MAX_CPUS {
                if left & 1 << c != 0 && group(crate::platform::cpu_mpidr(c)) == group(t) {
                    list |= 1 << (crate::platform::cpu_mpidr(c) & 0xF);
                    left &= !(1 << c);
                }
            }
            let v = list | (t >> 8 & 0xFF) << 16 | (id as u64) << 24 | (t >> 16 & 0xFF) << 32 | (t >> 4 & 0xF) << 44 | (t >> 32 & 0xFF) << 48;
            // SAFETY: ICC_SGI1R_EL1 (write-only): raises SGI `id` at the cores in the target list; no memory is accessed.
            unsafe { asm!("msr S3_0_C12_C11_5, {}", in(reg) v, options(nomem, nostack)) };
        }
        // SAFETY: barrier only.
        unsafe { asm!("isb", options(nomem, nostack)) };
    }

    fn enable(id: u32) {
        if id < 32 {
            w32(rd() + SGI_BASE + 0x100, 1 << id);
        } else {
            w32(gicd() + 0x100 + 4 * (id as usize / 32), 1 << (id % 32));
        }
    }

    fn disable(id: u32) {
        if id < 32 {
            w32(rd() + SGI_BASE + 0x180, 1 << id);
        } else {
            w32(gicd() + 0x180 + 4 * (id as usize / 32), 1 << (id % 32));
        }
    }

    fn configure_spi(id: u32) {
        Self::disable(id);
        let d = gicd();
        w8(d + 0x400 + id as usize, 0x80); // GICD_IPRIORITYR
        w64(d + 0x6000 + 8 * id as usize, crate::platform::cpu_mpidr(0) & 0xFF_00FF_FFFF); // GICD_IROUTER: the boot core
    }

    fn set_urgent(id: u32) {
        w8(gicd() + 0x400 + id as usize, 0x40);
    }

    fn set_edge(id: u32) {
        let reg = gicd() + 0xC00 + 4 * (id as usize / 16);
        w32(reg, r32(reg) | 2 << (2 * (id % 16)));
    }

    fn ack() -> u32 {
        let iar: u64;
        // SAFETY: ICC_IAR1_EL1: acknowledges the highest-priority pending Group 1 interrupt, by design; no memory is accessed.
        unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar, options(nomem, nostack)) };
        iar as u32
    }

    fn eoi(iar: u32) {
        // SAFETY: ICC_EOIR1_EL1: ends the interrupt `iar` named; no memory is accessed.
        unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) iar as u64, options(nomem, nostack)) };
    }
}
