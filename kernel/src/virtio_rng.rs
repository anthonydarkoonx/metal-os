//! Entropy from a virtio-rng device (virtio 1.x, entropy device: one queue, the device fills a buffer the driver offers),
//! written from the virtio specification's PCI transport and split-virtqueue chapters.
//!
//! The kernel reads 32 bytes once, while it probes the bus, folds them into the layout generator (`rand.rs`), and shuts
//! the device down again: nothing stays mapped or running. The device sits behind its own SMMU stream like every other
//! DMA device and reaches one page, the one holding the ring and the buffer.
use crate::{cache, mem, obj, pci, smmu};
use core::ptr::{read_volatile, write_volatile};

const SID_ROLE: u8 = 0; // no driver task owns this stream
const WAIT_TICKS: u64 = 50; // a device that does not answer in this many milliseconds is given up on
const BYTES: usize = 32;

// virtio_pci_common_cfg
const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0c;
const DEVICE_STATUS: usize = 0x14;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_ENABLE: usize = 0x1c;
const QUEUE_NOTIFY_OFF: usize = 0x1e;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;

const COMMON_LEN: usize = 0x38; // the common configuration structure ends after queue_used (0x30 + 8)

const ACKNOWLEDGE: u8 = 1;
const DRIVER: u8 = 2;
const DRIVER_OK: u8 = 4;
const FEATURES_OK: u8 = 8;

// the one page: descriptor table, available ring, used ring, then the buffer
const QSIZE: u16 = 4;
const AVAIL: usize = 0x100;
const USED: usize = 0x200;
const DATA: usize = 0x400;

fn counter() -> (u64, u64) {
    let (c, f): (u64, u64);
    // SAFETY: reads CNTPCT_EL0 and CNTFRQ_EL0, both readable at EL1; no memory is accessed.
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", "mrs {}, cntfrq_el0", out(reg) c, out(reg) f, options(nomem, nostack)) };
    (c, f)
}

/// Read entropy from `f` (a virtio-rng function) and give it to `rand`. Logs what happened.
pub fn collect(f: &pci::Func, cursor: &mut usize) {
    if !crate::rand::wants_entropy() {
        crate::kprintln!("[rand] virtio-rng not read: layouts are meant to follow from the seed alone (or are not randomised)");
        return;
    }
    match read(f, cursor) {
        Some(bytes) => {
            crate::rand::add_entropy(&bytes);
            crate::kprintln!("[rand] virtio-rng: {BYTES} bytes folded into the layout generator");
        }
        None => crate::kprintln!("[rand] virtio-rng present but gave no entropy"),
    }
}

fn read(f: &pci::Func, cursor: &mut usize) -> Option<[u8; BYTES]> {
    let mut bars = [(0usize, 0usize); 6];
    pci::assign_bars(f, cursor, &mut bars);
    let (cb, coff, _, _) = pci::virtio_cap(f, 1)?;
    let (nb, noff, _, mult) = pci::virtio_cap(f, 2)?;
    let (common, notify) = (bars[cb as usize % 6].0 + coff, bars[nb as usize % 6].0 + noff);
    // The offsets come from the device's own capability list: they must lie inside the BARs it was given before any
    // register is touched (a bad one would make the kernel read or write arbitrary physical addresses).
    let (clen, nlen) = (bars[cb as usize % 6].1, bars[nb as usize % 6].1);
    if clen == 0 || nlen == 0 || coff.checked_add(COMMON_LEN)? > clen || noff.checked_add(2)? > nlen || coff % 4 != 0 || noff % 2 != 0 || mult % 2 != 0 {
        return None;
    }
    let notify_end = bars[nb as usize % 6].0 + nlen;
    pci::enable(f);
    pci::disable_intx(f);
    let page = mem::alloc(mem::OWNER_KERNEL)?;
    // SAFETY: `page` was just returned by `mem::alloc` for the kernel: a whole, aligned, RAM frame owned here; the device is not attached yet.
    unsafe { core::ptr::write_bytes(page as *mut u8, 0, mem::PAGE) };
    obj::set_rng_dma(page);
    smmu::attach((f.dev as u16) << 3, SID_ROLE, &[obj::OBJ_RNG_DMA]);
    let out = exchange(common, notify, notify_end, mult, page);
    // shut the device down and take its page away again
    w8(common + DEVICE_STATUS, 0);
    // The reset is finished when the status reads 0 (virtio 1.x, 2.1.2); until then the device may still write the page.
    let (start, freq) = counter();
    let mut reset = false;
    while (counter().0 - start) * 1000 <= WAIT_TICKS * freq {
        if r8(common + DEVICE_STATUS) == 0 {
            reset = true;
            break;
        }
    }
    smmu::detach((f.dev as u16) << 3);
    obj::clear_rng_dma();
    let cmd = f.r32(4);
    f.w32(4, cmd & 0xffff & !0b110); // no more bus mastering
    if reset {
        mem::free(page);
    } // else: a device that never completes its reset keeps its page (leaked, never handed to anyone else)
    out
}

fn r8(a: usize) -> u8 {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`) in the device block; byte access needs no alignment; volatile MMIO.
    unsafe { read_volatile(a as *const u8) }
}
fn w8(a: usize, v: u8) {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`) in the device block; byte access needs no alignment; volatile MMIO.
    unsafe { write_volatile(a as *mut u8, v) }
}
fn r32(a: usize) -> u32 {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`), aligned for this width (the BAR base is page aligned and `read` refuses unaligned capability offsets); volatile MMIO.
    unsafe { read_volatile(a as *const u32) }
}
fn w32(a: usize, v: u32) {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`), aligned for this width (the BAR base is page aligned and `read` refuses unaligned capability offsets); volatile MMIO.
    unsafe { write_volatile(a as *mut u32, v) }
}
fn w16(a: usize, v: u16) {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`), aligned for this width (the BAR base is page aligned and `read` refuses unaligned capability offsets); volatile MMIO.
    unsafe { write_volatile(a as *mut u16, v) }
}
fn r16(a: usize) -> u16 {
    // SAFETY: `a` is a register inside the BAR (bounds checked in `read`), aligned for this width (the BAR base is page aligned and `read` refuses unaligned capability offsets); volatile MMIO.
    unsafe { read_volatile(a as *const u16) }
}
fn w64(a: usize, v: u64) { w32(a, v as u32); w32(a + 4, (v >> 32) as u32) }

fn exchange(common: usize, notify: usize, notify_end: usize, mult: u32, page: usize) -> Option<[u8; BYTES]> {
    w8(common + DEVICE_STATUS, 0);
    w8(common + DEVICE_STATUS, ACKNOWLEDGE);
    w8(common + DEVICE_STATUS, ACKNOWLEDGE | DRIVER);
    // features: VERSION_1 (bit 32) is required; the device sits behind an IOMMU, so ACCESS_PLATFORM (bit 33) is accepted if offered
    w32(common + DEVICE_FEATURE_SELECT, 1);
    let offered = r32(common + DEVICE_FEATURE);
    if offered & 1 == 0 {
        return None;
    }
    w32(common + DRIVER_FEATURE_SELECT, 0);
    w32(common + DRIVER_FEATURE, 0);
    w32(common + DRIVER_FEATURE_SELECT, 1);
    w32(common + DRIVER_FEATURE, offered & 0b11);
    w8(common + DEVICE_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
    if r8(common + DEVICE_STATUS) & FEATURES_OK == 0 {
        return None;
    }
    w16(common + QUEUE_SELECT, 0);
    let max = r16(common + QUEUE_SIZE);
    if max == 0 {
        return None;
    }
    w16(common + QUEUE_SIZE, QSIZE.min(max));
    w64(common + QUEUE_DESC, page as u64);
    w64(common + QUEUE_DRIVER, (page + AVAIL) as u64);
    w64(common + QUEUE_DEVICE, (page + USED) as u64);
    let notify_off = r16(common + QUEUE_NOTIFY_OFF) as usize;
    w16(common + QUEUE_ENABLE, 1);
    w8(common + DEVICE_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
    // one descriptor: the device writes BYTES bytes at DATA
    // SAFETY: `page` is the kernel-owned frame (`read` allocated it); descriptor at 0, avail at AVAIL, all aligned and well inside the page;
    // the device is not told yet (the kick comes later), so the writes cannot race with it.
    unsafe {
        write_volatile(page as *mut u64, (page + DATA) as u64);
        write_volatile((page + 8) as *mut u32, BYTES as u32);
        write_volatile((page + 12) as *mut u16, 2); // VIRTQ_DESC_F_WRITE
        write_volatile((page + AVAIL + 4) as *mut u16, 0); // ring[0] = descriptor 0
        write_volatile((page + AVAIL + 2) as *mut u16, 1); // idx
    }
    cache::clean(page, mem::PAGE);
    let kick = notify.checked_add(notify_off.checked_mul(mult as usize)?)?;
    if kick.checked_add(2)? > notify_end {
        return None;
    }
    w16(kick, 0);
    let (start, freq) = counter();
    loop {
        cache::invalidate(page + USED, 8);
        // SAFETY: `page + USED + 2` is the used-ring idx (2-aligned) inside the kernel-owned frame; volatile because the device writes it.
        if unsafe { read_volatile((page + USED + 2) as *const u16) } != 0 {
            break;
        }
        if (counter().0 - start) * 1000 > WAIT_TICKS * freq {
            return None;
        }
    }
    cache::invalidate(page + USED, 16);
    // SAFETY: `page + USED + 8` is the first used element's len (4-aligned) inside the kernel-owned frame; volatile (device-written).
    let len = unsafe { read_volatile((page + USED + 8) as *const u32) } as usize;
    if len == 0 {
        return None;
    }
    cache::invalidate(page + DATA, BYTES);
    let mut out = [0u8; BYTES];
    for (i, b) in out.iter_mut().enumerate().take(len.min(BYTES)) {
        // SAFETY: `page + DATA + i` with i < BYTES lies in the kernel-owned frame (DATA = 0x400, BYTES well under a page); volatile (device-written).
        *b = unsafe { read_volatile((page + DATA + i) as *const u8) };
    }
    Some(out)
}
