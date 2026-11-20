//! QEMU `virt`: GICv2 distributor / CPU interface (GICD 0x0800_0000, GICC 0x0801_0000).
use crate::hal::IrqController;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Where the registers are (from the device tree; the QEMU `virt` addresses until it has been read).
pub static GICD_BASE: AtomicUsize = AtomicUsize::new(0x0800_0000);
pub static GICC_BASE: AtomicUsize = AtomicUsize::new(0x0801_0000);

fn w32(addr: usize, v: u32) {
    // SAFETY: every caller passes a GICD/GICC register (4-aligned) inside the device block `map_devices` maps; volatile MMIO write.
    unsafe { write_volatile(addr as *mut u32, v) };
}
fn w8(addr: usize, v: u8) {
    // SAFETY: callers pass a byte-wide GICD priority/target register inside the mapped device block; volatile MMIO write.
    unsafe { write_volatile(addr as *mut u8, v) };
}

fn gicd() -> usize {
    GICD_BASE.load(Ordering::Relaxed)
}
fn gicc() -> usize {
    GICC_BASE.load(Ordering::Relaxed)
}

pub struct GicV2;

impl IrqController for GicV2 {
    fn init_dist() {
        w32(gicd(), 1); // GICD_CTLR
    }

    fn init_cpu() {
        w32(gicc() + 0x004, 0xFF); // GICC_PMR
        w32(gicc(), 1); // GICC_CTLR
        w32(gicd() + 0x100, 0xFFFF | 1 << 30); // GICD_ISENABLER0 (banked): SGIs 0..15 + timer PPI
    }

    fn send_sgi_to(mask: u32, id: u32) {
        // Whatever the sender wrote for the target (the work it will look for) must be visible before the interrupt is.
        // SAFETY: barrier only; orders the sender's earlier stores before the SGI write.
        unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
        w32(gicd() + 0xF00, (mask & 0xFF) << 16 | id); // GICD_SGIR, TargetListFilter = target list
    }

    fn enable(id: u32) {
        w32(gicd() + 0x100 + 4 * (id as usize / 32), 1 << (id % 32));
    }

    fn disable(id: u32) {
        w32(gicd() + 0x180 + 4 * (id as usize / 32), 1 << (id % 32));
    }

    fn configure_spi(id: u32) {
        Self::disable(id);
        w8(gicd() + 0x400 + id as usize, 0x80); // GICD_IPRIORITYR
        w8(gicd() + 0x800 + id as usize, 0x01); // GICD_ITARGETSR: CPU0
    }

    fn set_urgent(id: u32) {
        w8(gicd() + 0x400 + id as usize, 0x40);
    }

    fn set_edge(id: u32) {
        let reg = gicd() + 0xC00 + 4 * (id as usize / 16);
        // SAFETY: `reg` is a GICD_ICFGR word (4-aligned, 0xC00 + 4*(id/16)) in the mapped device block; volatile MMIO read.
        let v = unsafe { read_volatile(reg as *const u32) };
        w32(reg, v | 2 << (2 * (id % 16))); // GICD_ICFGR: two bits per interrupt, the upper one selects edge
    }

    fn ack() -> u32 {
        // SAFETY: GICC_IAR (GICC+0xC), a 4-aligned register in the mapped device block; the read acknowledges the interrupt, by design.
        unsafe { read_volatile((gicc() + 0x00C) as *const u32) }
    }

    fn eoi(iar: u32) {
        w32(gicc() + 0x010, iar);
    }
}
