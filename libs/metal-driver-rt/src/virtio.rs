//! virtio 1.x over PCI, shared by the userspace device drivers. Written from the OASIS virtio
//! specification: vendor capabilities locate the common / notification / ISR / device structures
//! inside a BAR; a split virtqueue is a descriptor table, an available ring and a used ring.
//!
//! The kernel hands a driver the BAR that holds the registers and a read-only copy of the
//! device's PCI configuration space; everything else is parsed here.
use crate::dma;
use core::ptr::{read_volatile, write_volatile};

// common configuration structure
const C_DEVFEAT_SEL: usize = 0x00;
const C_DEVFEAT: usize = 0x04;
const C_DRVFEAT_SEL: usize = 0x08;
const C_DRVFEAT: usize = 0x0c;
const C_STATUS: usize = 0x14;
const C_QSEL: usize = 0x16;
const C_QSIZE: usize = 0x18;
const C_QENABLE: usize = 0x1c;
const C_QNOTIFY_OFF: usize = 0x1e;
const C_QDESC: usize = 0x20;
const C_QAVAIL: usize = 0x28;
const C_QUSED: usize = 0x30;

const ST_ACK: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_DRIVER_OK: u8 = 4;
const ST_FEATURES_OK: u8 = 8;

// split virtqueue layout inside one page: descriptors, available ring, used ring
pub const OFF_DESC: usize = 0x000;
pub const OFF_AVAIL: usize = 0x100;
pub const OFF_USED: usize = 0x200;

/// Where `queue_sized` puts the available and the used ring (offsets from the start of the queue's page), or None if the queue does not fit
/// in one page. The used ring starts on a 64-byte boundary, and the available ring ends in an earlier cache line.
pub fn packed_layout(size: u16) -> Option<(usize, usize)> {
    let avail = 16 * size as usize;
    let used = (avail + 6 + 2 * size as usize + 63) & !63;
    (used + 6 + 8 * size as usize <= 4096).then_some((avail, used))
}

fn rd32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn wr32(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn wr64(a: usize, v: u64) {
    wr32(a, v as u32);
    wr32(a + 4, (v >> 32) as u32);
}
fn rd16(a: usize) -> u16 {
    unsafe { read_volatile(a as *const u16) }
}
fn wr16(a: usize, v: u16) {
    unsafe { write_volatile(a as *mut u16, v) }
}
pub fn fence() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// The structures of one virtio PCI device, as addresses in this task's mapping of its BAR.
pub struct Pci {
    common: usize,
    notify_base: usize,
    notify_mult: usize,
    isr: usize,
    devcfg: usize,
}

impl Pci {
    /// Parse the vendor capabilities in the config-space copy at `cfg_va`. `bar_va` / `bar_bytes`
    /// describe the mapped BAR; `device` is the PCI device id the driver expects.
    pub fn open(bar_va: usize, bar_bytes: usize, cfg_va: usize, device: u16) -> Result<Pci, &'static str> {
        if rd32(cfg_va) != (device as u32) << 16 | 0x1af4 {
            return Err("not the expected virtio device");
        }
        let b = |o: usize| unsafe { read_volatile((cfg_va + o) as *const u8) } as usize;
        let w = |o: usize| rd32(cfg_va + o) as usize;
        if b(0x06) & 0x10 == 0 {
            return Err("no capability list");
        }
        let (mut common, mut notify, mut mult, mut isr, mut dev) = (None, None, 0, None, None);
        let mut ptr = b(0x34) & !3;
        for _ in 0..48 {
            if ptr < 0x40 || ptr + 20 > 256 {
                break;
            }
            if b(ptr) == 0x09 {
                let (ty, off, len) = (b(ptr + 3), w(ptr + 8), w(ptr + 12));
                if off.checked_add(len).map_or(true, |e| e > bar_bytes) {
                    return Err("capability outside the BAR");
                }
                match ty {
                    1 => common = Some(off),
                    2 => (notify, mult) = (Some(off), w(ptr + 16)),
                    3 => isr = Some(off),
                    4 => dev = Some(off),
                    _ => {}
                }
            }
            ptr = b(ptr + 1) & !3;
        }
        match (common, notify, isr, dev) {
            (Some(c), Some(n), Some(i), Some(d)) => {
                Ok(Pci { common: bar_va + c, notify_base: bar_va + n, notify_mult: mult, isr: bar_va + i, devcfg: bar_va + d })
            }
            _ => Err("missing virtio capability"),
        }
    }

    fn status(&self) -> u8 {
        unsafe { read_volatile((self.common + C_STATUS) as *const u8) }
    }
    fn set_status(&self, v: u8) {
        unsafe { write_volatile((self.common + C_STATUS) as *mut u8, v) }
    }

    /// Reset the device (discarding anything a crashed predecessor left behind) and negotiate:
    /// VERSION_1 (bit 32) and ACCESS_PLATFORM (bit 33, the device's DMA goes through the
    /// platform's IOMMU, which is how the kernel confines it), nothing else.
    pub fn start(&self) -> Result<(), &'static str> {
        self.start_with(0)
    }

    /// `start`, also accepting the device-specific feature bits `extra` (bits 0..31) the device offers.
    pub fn start_with(&self, extra: u32) -> Result<(), &'static str> {
        self.set_status(0);
        self.set_status(ST_ACK);
        self.set_status(ST_ACK | ST_DRIVER);
        wr32(self.common + C_DEVFEAT_SEL, 1);
        let feat = rd32(self.common + C_DEVFEAT);
        if feat & 1 == 0 {
            return Err("device lacks VIRTIO_F_VERSION_1");
        }
        if feat & 2 == 0 {
            return Err("device lacks VIRTIO_F_ACCESS_PLATFORM");
        }
        wr32(self.common + C_DEVFEAT_SEL, 0);
        let low = rd32(self.common + C_DEVFEAT);
        if low & extra != extra {
            return Err("device lacks a feature the driver needs");
        }
        wr32(self.common + C_DRVFEAT_SEL, 0);
        wr32(self.common + C_DRVFEAT, extra);
        wr32(self.common + C_DRVFEAT_SEL, 1);
        wr32(self.common + C_DRVFEAT, 0b11);
        self.set_status(ST_ACK | ST_DRIVER | ST_FEATURES_OK);
        if self.status() & ST_FEATURES_OK == 0 {
            return Err("device rejected features");
        }
        Ok(())
    }

    /// Set up virtqueue `index` of `size` entries in the page at `va` / `pa` (layout above).
    pub fn queue(&self, index: u16, size: u16, va: usize, pa: u64) -> Result<Queue, &'static str> {
        self.queue_at(index, size, va, pa, OFF_AVAIL, OFF_USED)
    }

    /// A queue of up to a few hundred entries: the three structures are packed one after the other from the start of
    /// the memory (descriptors, then the available ring, then the used ring), which must be zeroed and is one page. The used
    /// ring starts on a 64-byte boundary of its own: the driver cleans the available ring's cache lines and the device writes the used
    /// ring, so the two must never share a line (a clean would write the CPU's old copy of the used ring's header over the device's).
    pub fn queue_sized(&self, index: u16, size: u16, va: usize, pa: u64) -> Result<Queue, &'static str> {
        let (avail, used) = packed_layout(size).ok_or("queue does not fit")?;
        self.queue_at(index, size, va, pa, avail, used)
    }

    fn queue_at(&self, index: u16, size: u16, va: usize, pa: u64, off_avail: usize, off_used: usize) -> Result<Queue, &'static str> {
        wr16(self.common + C_QSEL, index);
        if rd16(self.common + C_QSIZE) < size {
            return Err("queue too small");
        }
        wr16(self.common + C_QSIZE, size);
        let off = rd16(self.common + C_QNOTIFY_OFF) as usize;
        wr64(self.common + C_QDESC, pa + OFF_DESC as u64);
        wr64(self.common + C_QAVAIL, pa + off_avail as u64);
        wr64(self.common + C_QUSED, pa + off_used as u64);
        wr16(self.common + C_QENABLE, 1);
        Ok(Queue {
            va,
            pa,
            size,
            index,
            off_avail,
            off_used,
            avail_idx: 0,
            used_seen: 0,
            notify: self.notify_base + off * self.notify_mult,
            inflight: [(0, 0); MAX_CHAIN],
            ninflight: 0,
            posted: [(0, 0); MAX_POSTED],
        })
    }

    pub fn driver_ok(&self) {
        self.set_status(ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
    }

    /// Reading the ISR status acknowledges the interrupt.
    pub fn ack_isr(&self) {
        unsafe { read_volatile(self.isr as *const u8) };
    }

    /// A byte of the device-specific configuration.
    pub fn config8(&self, off: usize) -> u8 {
        unsafe { read_volatile((self.devcfg + off) as *const u8) }
    }

    pub fn set_config8(&self, off: usize, v: u8) {
        unsafe { write_volatile((self.devcfg + off) as *mut u8, v) }
    }

    /// A 32-bit word of the device-specific configuration.
    pub fn config32(&self, off: usize) -> u32 {
        rd32(self.devcfg + off)
    }
}

/// One buffer of a descriptor chain: where the device finds it (`pa`), where this task sees it (`va`, for cache
/// maintenance; 0 = not mapped here, nothing to maintain), its length, and whether the device writes it.
#[derive(Clone, Copy)]
pub struct Buf {
    pub pa: u64,
    pub va: usize,
    pub len: u32,
    pub dev_writes: bool,
}

const MAX_CHAIN: usize = 4;
const MAX_POSTED: usize = 64;

/// One split virtqueue, driven synchronously: one chain of descriptors in flight at a time.
///
/// Every transfer is wrapped in the cache maintenance the module `dma` describes: the driver never has to
/// remember it, because buffers are handed to the queue with their addresses.
pub struct Queue {
    va: usize,
    pa: u64,
    size: u16,
    index: u16,
    off_avail: usize,
    off_used: usize,
    avail_idx: u16,
    used_seen: u16,
    notify: usize,
    /// device-written buffers of the chain in flight: invalidated again once it completes
    inflight: [(usize, u32); MAX_CHAIN],
    ninflight: usize,
    /// device-written buffer of each posted descriptor (queues the device fills on its own schedule)
    posted: [(usize, u32); MAX_POSTED],
}

impl Queue {
    /// Queue a chain of buffers and notify the device.
    pub fn submit(&mut self, chain: &[Buf]) {
        self.submit_quiet(chain);
        fence();
        wr16(self.notify, self.index);
    }

    /// Publish a chain without notifying the device: it will not look at it. (Tests use this to model
    /// a device that never answers.)
    pub fn submit_quiet(&mut self, chain: &[Buf]) {
        const NEXT: u16 = 1;
        const WRITE: u16 = 2;
        self.ninflight = 0;
        for b in chain {
            // what the device reads must be in memory; where it writes, no dirty line may survive to overwrite it
            if b.dev_writes {
                dma::flush(b.va, b.len as usize);
                if b.va != 0 && self.ninflight < MAX_CHAIN {
                    self.inflight[self.ninflight] = (b.va, b.len);
                    self.ninflight += 1;
                }
            } else {
                dma::clean(b.va, b.len as usize);
            }
        }
        for (i, b) in chain.iter().enumerate() {
            let last = i + 1 == chain.len();
            let d = (self.va + OFF_DESC + 16 * i) as *mut u8;
            unsafe {
                write_volatile(d as *mut u64, b.pa);
                write_volatile(d.add(8) as *mut u32, b.len);
                write_volatile(d.add(12) as *mut u16, if last { 0 } else { NEXT } | if b.dev_writes { WRITE } else { 0 });
                write_volatile(d.add(14) as *mut u16, if last { 0 } else { i as u16 + 1 });
            }
        }
        dma::clean(self.va + OFF_DESC, 16 * chain.len());
        unsafe {
            let avail = (self.va + self.off_avail) as *mut u16;
            write_volatile(avail.add(2 + (self.avail_idx % self.size) as usize), 0); // head = descriptor 0
            fence();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            write_volatile(avail.add(1), self.avail_idx);
        }
        dma::clean(self.va + self.off_avail, 4 + 2 * self.size as usize);
    }

    /// Has the device finished the chain submitted last? (Call after an interrupt.)
    pub fn done(&mut self) -> bool {
        fence();
        dma::flush(self.va + self.off_used, 4 + 8 * self.size as usize); // the device wrote the used ring
        let used = unsafe { read_volatile((self.va + self.off_used + 2) as *const u16) };
        if used != self.used_seen {
            fence(); // the device's data is read only after its index is seen
            self.used_seen = used;
            for i in 0..self.ninflight {
                let (va, len) = self.inflight[i];
                dma::flush(va, len as usize); // the data the device wrote, not what the cache held
            }
            self.ninflight = 0;
            fence();
            return true;
        }
        false
    }

    pub fn pa(&self) -> u64 {
        self.pa
    }

    /// Hand the device one standalone buffer (descriptor `desc`), for queues the *device* fills
    /// on its own schedule, such as an input device's event queue. Call `kick` after a batch.
    pub fn post(&mut self, desc: u16, b: Buf) {
        if b.dev_writes {
            dma::flush(b.va, b.len as usize);
            if let Some(p) = self.posted.get_mut(desc as usize) {
                *p = (b.va, b.len);
            }
        } else {
            dma::clean(b.va, b.len as usize);
        }
        let d = (self.va + OFF_DESC + 16 * desc as usize) as *mut u8;
        unsafe {
            write_volatile(d as *mut u64, b.pa);
            write_volatile(d.add(8) as *mut u32, b.len);
            write_volatile(d.add(12) as *mut u16, if b.dev_writes { 2 } else { 0 });
            write_volatile(d.add(14) as *mut u16, 0);
        }
        dma::clean(self.va + OFF_DESC + 16 * desc as usize, 16);
        unsafe {
            let avail = (self.va + self.off_avail) as *mut u16;
            write_volatile(avail.add(2 + (self.avail_idx % self.size) as usize), desc);
            fence();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            write_volatile(avail.add(1), self.avail_idx);
        }
        dma::clean(self.va + self.off_avail, 4 + 2 * self.size as usize);
    }

    pub fn kick(&self) {
        fence();
        wr16(self.notify, self.index);
    }

    /// The next buffer the device has finished with: (descriptor id, bytes written).
    pub fn next_used(&mut self) -> Option<(u16, u32)> {
        fence();
        dma::flush(self.va + self.off_used, 4 + 8 * self.size as usize); // the device wrote the used ring
        let idx = unsafe { read_volatile((self.va + self.off_used + 2) as *const u16) };
        if idx == self.used_seen {
            return None;
        }
        fence(); // the element is read only after its index is seen ...
        let e = (self.va + self.off_used + 4 + 8 * (self.used_seen % self.size) as usize) as *const u32;
        dma::flush(e as usize, 8); // ... and from memory, not from a line fetched before the device wrote it
        let (id, len) = unsafe { (read_volatile(e), read_volatile(e.add(1))) };
        self.used_seen = self.used_seen.wrapping_add(1);
        if let Some(&(va, n)) = self.posted.get(id as usize) {
            dma::flush(va, n as usize); // the event the device wrote into the buffer, not a stale copy
        }
        fence();
        Some((id as u16, len))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn the_used_ring_never_shares_a_cache_line_with_the_available_ring() {
        for size in 1..=256u16 {
            let Some((avail, used)) = packed_layout(size) else { continue };
            let avail_last_byte = avail + 4 + 2 * size as usize + 2 - 1; // flags, idx, the entries, used_event
            assert_eq!(used % 64, 0, "size {size}: used ring not line-aligned");
            assert!(avail_last_byte / 64 < used / 64, "size {size}: the rings share a cache line");
            assert!(avail >= 16 * size as usize, "size {size}: overlaps the descriptors");
            assert!(used + 4 + 8 * size as usize + 2 <= 4096, "size {size}: past the page");
        }
    }

    #[test]
    fn the_network_queues_fit_one_page_each() {
        use crate::netring::{NRX, NTX};
        assert!(packed_layout(NRX as u16).is_some());
        assert!(packed_layout(NTX as u16).is_some());
        // (the transmit queue starts one page after the receive queue)
        let (_, used) = packed_layout(NRX as u16).unwrap();
        assert!(used + 6 + 8 * NRX <= 4096);
    }

    #[test]
    fn a_queue_too_big_for_a_page_is_refused() {
        assert!(packed_layout(512).is_none());
    }
}
