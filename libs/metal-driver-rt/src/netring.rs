//! The ring page the network driver and `netd` share, and the packet buffers next to it.
//!
//! Four single-producer single-consumer rings of 64 entries, each a producer index and a consumer index (free-running
//! `u32`s, in separate cache lines) followed by the entries:
//!
//! | ring | producer | consumer | entry |
//! |---|---|---|---|
//! | RXQ | driver | netd | receive slot \| frame length (with the 12-byte virtio header) << 16 |
//! | RXFREE | netd | driver | receive slot, finished with: the driver gives it back to the device |
//! | TXQ | netd | driver | transmit slot \| frame length << 16 (the frame starts 12 bytes into the slot) |
//! | TXDONE | driver | netd | transmit slot, the device has sent it |
//!
//! Slots are 2 KiB pieces of the shared buffer area: 64 receive slots first, then 32 transmit slots. Words after the
//! rings: the device's MAC address, a "driver is up" word and an epoch that changes whenever the driver (re)starts.
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

pub const SLOT: usize = 2048;
pub const NRX: usize = 64;
pub const NTX: usize = 32;
pub const HDR: usize = 12;
pub const RING_ENTRIES: usize = 64;

pub const RXQ: usize = 0x000;
pub const RXFREE: usize = 0x200;
pub const TXQ: usize = 0x400;
pub const TXDONE: usize = 0x600;
/// MAC address (6 bytes), then `READY` (u32), `EPOCH` (u32).
pub const INFO: usize = 0x800;
pub const READY: usize = INFO + 8;
pub const EPOCH: usize = INFO + 12;
/// How many ticks the driver may sleep before it must wake `netd` again (written by `netd` from its next timer: an idle
/// network lets the whole machine sleep).
pub const POLL: usize = INFO + 16;

/// One end of a ring at `base` (the ring page's address in this task plus the ring's offset).
#[derive(Clone, Copy)]
pub struct Ring(pub usize);

impl Ring {
    fn prod(&self) -> *mut u32 {
        self.0 as *mut u32
    }
    fn cons(&self) -> *mut u32 {
        (self.0 + 64) as *mut u32
    }
    fn entry(&self, i: u32) -> *mut u32 {
        (self.0 + 128 + 4 * (i as usize % RING_ENTRIES)) as *mut u32
    }

    pub fn len(&self) -> u32 {
        fence(Ordering::SeqCst);
        unsafe { read_volatile(self.prod()).wrapping_sub(read_volatile(self.cons())).min(RING_ENTRIES as u32) }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() as usize >= RING_ENTRIES
    }

    /// Producer: add an entry (false if full).
    pub fn push(&self, v: u32) -> bool {
        if self.is_full() {
            return false;
        }
        unsafe {
            let p = read_volatile(self.prod());
            write_volatile(self.entry(p), v);
            fence(Ordering::SeqCst);
            write_volatile(self.prod(), p.wrapping_add(1));
        }
        true
    }

    /// Consumer: take the oldest entry.
    pub fn pop(&self) -> Option<u32> {
        if self.is_empty() {
            return None;
        }
        unsafe {
            let c = read_volatile(self.cons());
            let v = read_volatile(self.entry(c));
            fence(Ordering::SeqCst);
            write_volatile(self.cons(), c.wrapping_add(1));
            Some(v)
        }
    }

    /// Forget everything (the producer's side restarting).
    pub fn reset(&self) {
        unsafe {
            write_volatile(self.prod(), 0);
            write_volatile(self.cons(), 0);
        }
    }
}

pub fn entry(slot: usize, len: usize) -> u32 {
    slot as u32 | (len as u32) << 16
}

pub fn unpack(v: u32) -> (usize, usize) {
    ((v & 0xFFFF) as usize, (v >> 16) as usize)
}
