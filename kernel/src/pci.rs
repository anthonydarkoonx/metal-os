//! PCIe enumeration for QEMU `virt`'s generic ECAM host bridge, written from the PCI Express
//! base specification (configuration header layout, BAR sizing) and the virtio 1.x PCI
//! transport (vendor capability layout). Firmware does not run before the kernel, so the
//! kernel assigns BARs itself, from the host bridge's 32-bit memory window.
use core::ptr::{read_volatile, write_volatile};

/// Base of the enhanced configuration access mechanism region (QEMU `virt` memory map).
pub const ECAM: usize = 0x40_1000_0000;
/// The host bridge's 32-bit memory window (from the machine's device tree).
const WINDOW_BASE: usize = 0x1000_0000;
const WINDOW_END: usize = 0x3eff_0000;

pub const VIRTIO_VENDOR: u16 = 0x1af4;
/// Transitional-free ("modern") virtio device ids are 0x1040 + the virtio device type.
pub const VIRTIO_NET_MODERN: u16 = 0x1040 + 1;
pub const VIRTIO_RNG_MODERN: u16 = 0x1040 + 4;
pub const VIRTIO_BLK_MODERN: u16 = 0x1040 + 2;
pub const VIRTIO_GPU_MODERN: u16 = 0x1040 + 16;
pub const VIRTIO_INPUT_MODERN: u16 = 0x1040 + 18;

/// The xHCI USB controller QEMU emulates (`-device qemu-xhci`): Red Hat's vendor id and its device id. Its registers are in BAR 0.
pub const XHCI: (u16, u16) = (0x1b36, 0x000d);

const CAP_VENDOR: u8 = 0x09;

/// A function at bus 0: `dev` is the device number (function 0 only).
#[derive(Clone, Copy)]
pub struct Func {
    pub dev: u8,
}

impl Func {
    fn addr(&self, off: usize) -> usize {
        ECAM + ((self.dev as usize) << 15) + off
    }
    pub fn r32(&self, off: usize) -> u32 {
        // SAFETY: `addr` is ECAM + dev<<15 + off, inside the 1 GiB ECAM device block `map_devices` maps (l1[256]); callers pass a 4-aligned
        // `off` below 0x1000; volatile because it is device config space.
        unsafe { read_volatile(self.addr(off) as *const u32) }
    }
    pub fn w32(&self, off: usize, v: u32) {
        // SAFETY: as `r32`: an aligned config-space register inside the mapped ECAM device block; volatile MMIO write.
        unsafe { write_volatile(self.addr(off) as *mut u32, v) }
    }
    fn r16(&self, off: usize) -> u16 {
        (self.r32(off & !3) >> (8 * (off & 2))) as u16
    }
    fn r8(&self, off: usize) -> u8 {
        (self.r32(off & !3) >> (8 * (off & 3))) as u8
    }
    pub fn vendor(&self) -> u16 {
        self.r16(0)
    }
    pub fn device(&self) -> u16 {
        self.r16(2)
    }
    /// GIC SPI number of this function's INTx line. The host bridge swizzles the pin by the
    /// device number over four shared lines starting at SPI 3 (its interrupt-map).
    pub fn intx_spi(&self) -> Option<u32> {
        let pin = self.r8(0x3d) as u32;
        (1..=4).contains(&pin).then(|| 3 + (self.dev as u32 + pin - 1) % 4)
    }
}

/// Every function that answers at bus 0, device 0..31.
pub fn scan() -> impl Iterator<Item = Func> {
    (0..32u8).map(|dev| Func { dev }).filter(|f| f.vendor() != 0xffff)
}

/// Size and place every memory BAR of `f` in the window; returns the number of bytes used.
/// `cursor` is the next free window address. BAR `i` lands in `bars[i]` as (address, size).
pub fn assign_bars(f: &Func, cursor: &mut usize, bars: &mut [(usize, usize); 6]) {
    let mut i = 0;
    while i < 6 {
        let off = 0x10 + 4 * i;
        let orig = f.r32(off);
        if orig & 1 == 1 {
            i += 1; // I/O BAR: unused here
            continue;
        }
        let is64 = (orig >> 1) & 3 == 2;
        f.w32(off, 0xffff_ffff);
        let lo = f.r32(off) & !0xf;
        let size = if lo == 0 { 0 } else { (!lo).wrapping_add(1) as usize };
        if size != 0 {
            let at = (*cursor + size - 1) & !(size - 1);
            if at + size <= WINDOW_END {
                f.w32(off, at as u32);
                if is64 {
                    f.w32(off + 4, 0);
                }
                bars[i] = (at, size);
                *cursor = at + size;
            }
        } else {
            f.w32(off, orig);
        }
        i += if is64 { 2 } else { 1 };
    }
}

pub fn window_start() -> usize {
    WINDOW_BASE
}

/// Enable memory decoding and bus mastering (command register bits 1 and 2).
pub fn enable(f: &Func) {
    let cmd = f.r32(4);
    f.w32(4, (cmd & 0xffff) | 0b110);
}

/// Stop `f` from asserting its interrupt line (command register bit 10). For devices a driver polls: their line is shared
/// with others by the host bridge's swizzle, and a polled device that nobody acknowledges would hold it high.
pub fn disable_intx(f: &Func) {
    let cmd = f.r32(4);
    f.w32(4, (cmd & 0xffff) | 1 << 10);
}

/// BAR index shared by every virtio vendor capability of `f`, or None if the function has
/// none or they disagree.
pub fn virtio_bar(f: &Func) -> Option<u8> {
    if f.r16(6) & (1 << 4) == 0 {
        return None; // no capability list
    }
    let mut ptr = f.r8(0x34) as usize & !3;
    let mut bar = None;
    for _ in 0..48 {
        if ptr < 0x40 {
            break;
        }
        let (id, next) = (f.r8(ptr), f.r8(ptr + 1) as usize & !3);
        // cfg types 1..=4 locate structures in a BAR; type 5 (PCI config access) has no BAR
        if id == CAP_VENDOR && (1..=4).contains(&f.r8(ptr + 3)) {
            let b = f.r8(ptr + 4);
            if bar.is_some_and(|x| x != b) {
                return None;
            }
            bar = Some(b);
        }
        ptr = next;
    }
    bar
}

/// Copy the first 256 bytes of configuration space (after BAR assignment) to the RAM page at `pa` (bounds-checked).
pub fn snapshot(f: &Func, pa: usize) {
    let mut words = [0u8; 256];
    for i in 0..64 {
        words[4 * i..4 * i + 4].copy_from_slice(&f.r32(4 * i).to_le_bytes());
    }
    crate::frame::phys::copy_to(pa, &words);
}

/// The virtio vendor capability of type `cfg_type` (1 common, 2 notify, ...): (BAR index, offset in it, length, the word
/// after the length, which is the notify offset multiplier for type 2).
pub fn virtio_cap(f: &Func, cfg_type: u8) -> Option<(u8, usize, usize, u32)> {
    if f.r16(6) & (1 << 4) == 0 {
        return None;
    }
    let mut ptr = f.r8(0x34) as usize & !3;
    for _ in 0..48 {
        if ptr < 0x40 {
            break;
        }
        if f.r8(ptr) == CAP_VENDOR && f.r8(ptr + 3) == cfg_type {
            return Some((f.r8(ptr + 4), f.r32(ptr + 8) as usize, f.r32(ptr + 12) as usize, f.r32(ptr + 16)));
        }
        ptr = f.r8(ptr + 1) as usize & !3;
    }
    None
}
