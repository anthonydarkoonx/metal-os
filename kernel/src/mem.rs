//! Physical frame allocator over static per-frame tables, behind one lock.
//!
//! Every frame records its owner (so a crashed task's memory is reclaimed in
//! one pass), a kind, and, for movable pages, a reverse map to the single task
//! mapping that points at it. Kinds drive anti-fragmentation:
//!
//! * unmovable frames (kernel, page tables, DMA / shared objects) are taken
//!   from the *bottom* of RAM,
//! * movable frames (private user pages) from the *top*,
//!
//! so the two do not interleave and the compactor (compact.rs) can consolidate
//! the free space in between.
//!
//! The lock is held only to claim or release frames; zeroing a freshly claimed
//! frame happens after it is released (the frame is already ours and invisible
//! to everyone else).
#![forbid(unsafe_code)]
use crate::sync::{Guard, SpinLock, R_MEM};
use metal_compact::Frames;

pub const RAM_BASE: usize = crate::platform::RAM_BASE;
/// Tracked frames: up to 256 MiB of RAM.
pub const MAX_FRAMES: usize = 65536;
pub const PAGE: usize = 4096;

pub const OWNER_FREE: u8 = 0;
pub const OWNER_KERNEL: u8 = 1;
/// Task `tid` owns its frames as `OWNER_TASK0 + tid`.
pub const OWNER_TASK0: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FKind {
    Free = 0,
    /// Kernel image and data.
    Kernel = 1,
    /// Page-table frame (the MMU walks it; not relocatable).
    PageTable = 2,
    /// Device-visible or multiply-mapped: DMA buffers, shared objects.
    Pinned = 3,
    /// Private user page with exactly one mapping, recorded in the reverse map.
    Movable = 4,
}

pub struct MemState {
    owner: [u8; MAX_FRAMES],
    kind: [FKind; MAX_FRAMES],
    /// For movable frames: `tid << 32 | (virtual address >> 12)`. (The page number of an address in the 39-bit user space takes 27 bits, more
    /// than the 24 that fit beside a task number in 32: a position-independent image is placed as high as 256 GiB.)
    rmap: [u64; MAX_FRAMES],
    nframes: usize,
    /// No free frame exists below `low_hint`, nor above `high_hint`.
    low_hint: usize,
    high_hint: usize,
    free: usize,
}

static MEM: SpinLock<MemState> = SpinLock::new(MemState {
    owner: [OWNER_FREE; MAX_FRAMES],
    kind: [FKind::Free; MAX_FRAMES],
    rmap: [0; MAX_FRAMES],
    nframes: 0,
    low_hint: 0,
    high_hint: 0,
    free: 0,
});

/// Take the allocator lock (compaction needs it across several steps).
pub fn lock() -> Guard<'static, MemState> {
    MEM.lock(R_MEM)
}

impl MemState {
    fn claim(&mut self, i: usize, owner: u8, kind: FKind) -> usize {
        self.owner[i] = owner;
        self.kind[i] = kind;
        self.free -= 1;
        RAM_BASE + i * PAGE
    }

    fn release(&mut self, i: usize) {
        self.owner[i] = OWNER_FREE;
        self.kind[i] = FKind::Free;
        self.free += 1;
        self.low_hint = self.low_hint.min(i);
        self.high_hint = self.high_hint.max(i);
    }

    pub fn kind_at(&self, i: usize) -> FKind {
        self.kind[i]
    }

    pub fn rmap_at(&self, i: usize) -> (usize, usize) {
        let r = self.rmap[i];
        ((r >> 32) as usize, ((r & 0xFFFF_FFFF) as usize) << 12)
    }

    /// Relocation bookkeeping: `dst` takes over `src`'s role and `src` becomes free.
    pub fn move_meta(&mut self, src: usize, dst: usize) {
        self.owner[dst] = self.owner[src];
        self.kind[dst] = FKind::Movable;
        self.rmap[dst] = self.rmap[src];
        self.free -= 1; // `dst` was free and is now in use ...
        self.release(src); // ... while `src` becomes free (release counts it): the total is unchanged
    }
}

impl Frames for MemState {
    fn len(&self) -> usize {
        self.nframes
    }
    fn is_free(&self, i: usize) -> bool {
        self.kind[i] == FKind::Free
    }
    fn is_movable(&self, i: usize) -> bool {
        self.kind[i] == FKind::Movable
    }
}

/// `kernel` is where the kernel image is (`kaslr.rs` may have moved it into the middle of RAM). The memory below the
/// link address (QEMU puts the device tree there) stays reserved; wherever the image was before it moved is free.
pub fn init(ram_size: usize, kernel: core::ops::Range<usize>) {
    let mut m = lock();
    let n = (ram_size / PAGE).min(MAX_FRAMES);
    m.nframes = n;
    crate::frame::phys::set_ram_size(n * PAGE);
    let low = (crate::kaslr::LINK_BASE - RAM_BASE) / PAGE;
    let (k0, k1) = ((kernel.start - RAM_BASE) / PAGE, (kernel.end - RAM_BASE).div_ceil(PAGE));
    let mut free = 0;
    for i in 0..n {
        let kernel = i < low || (k0..k1).contains(&i);
        m.owner[i] = if kernel { OWNER_KERNEL } else { OWNER_FREE };
        m.kind[i] = if kernel { FKind::Kernel } else { FKind::Free };
        free += !kernel as usize;
    }
    m.low_hint = (0..n).find(|&i| m.kind[i] == FKind::Free).unwrap_or(n);
    m.high_hint = n.saturating_sub(1);
    m.free = free;
}

fn zero(pa: usize) {
    crate::frame::phys::zero_page(pa);
}

/// Allocate one zeroed *unmovable* frame from the bottom of RAM.
pub fn alloc_kind(owner: u8, kind: FKind) -> Option<usize> {
    debug_assert!(kind != FKind::Movable && kind != FKind::Free);
    let pa = {
        let mut m = lock();
        let n = m.nframes;
        let mut i = m.low_hint;
        while i < n && m.kind[i] != FKind::Free {
            i += 1;
        }
        if i >= n {
            return None;
        }
        m.low_hint = i + 1;
        m.claim(i, owner, kind)
    };
    zero(pa);
    Some(pa)
}

/// Allocate an unmovable kernel-owned frame.
pub fn alloc(owner: u8) -> Option<usize> {
    alloc_kind(owner, FKind::Kernel)
}

/// Allocate one zeroed *movable* frame (from the top of RAM) that will be mapped at `va` in `tid`.
pub fn alloc_movable(owner: u8, tid: usize, va: usize) -> Option<usize> {
    let pa = {
        let mut m = lock();
        let n = m.nframes;
        if n == 0 {
            return None;
        }
        let mut i = m.high_hint.min(n - 1);
        loop {
            if m.kind[i] == FKind::Free {
                break;
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
        m.high_hint = i.saturating_sub(1);
        m.rmap[i] = (tid as u64) << 32 | (va >> 12) as u64;
        m.claim(i, owner, FKind::Movable)
    };
    zero(pa);
    Some(pa)
}

/// Boot-time check of the reverse map: a page at the top of the user address space (where a position-independent image can be placed) is
/// recorded and read back as the same task and address.
pub fn rmap_selftest() -> bool {
    let (tid, va) = (crate::task::MAX_TASKS - 1, crate::task::PIE_HI - PAGE);
    let Some(pa) = alloc_movable(OWNER_KERNEL, tid, va) else { return false };
    let back = lock().rmap_at((pa - RAM_BASE) / PAGE);
    free(pa);
    back == (tid, va)
}

/// Is the frame at `pa` a private, relocatable user page (as opposed to shared or device memory, which never moves)?
pub fn is_movable(pa: usize) -> bool {
    // (an address outside RAM, a device page for one, is no frame at all, and never movable)
    let m = lock();
    pa.checked_sub(RAM_BASE).map_or(false, |d| m.kind.get(d / PAGE).map_or(false, |&k| k == FKind::Movable))
}

/// Who owns the frame at `pa` (`OWNER_FREE` if nobody).
pub fn owner_of(pa: usize) -> u8 {
    // (outside RAM there is no frame, so nobody owns it)
    let m = lock();
    pa.checked_sub(RAM_BASE).and_then(|d| m.owner.get(d / PAGE).copied()).unwrap_or(OWNER_FREE)
}

/// Hand a frame to another owner (shared memory that outlives the program that made it goes to the kernel).
pub fn set_owner(pa: usize, owner: u8) {
    let mut m = lock();
    let i = (pa - RAM_BASE) / PAGE;
    assert!(m.kind[i] != FKind::Free, "owner of a free frame");
    m.owner[i] = owner;
}

/// Free a single frame.
pub fn free(pa: usize) {
    let mut m = lock();
    let i = (pa - RAM_BASE) / PAGE;
    assert!(m.kind[i] != FKind::Free, "double free");
    m.release(i);
}

/// How many frames `owner` holds.
pub fn count_owned(owner: u8) -> usize {
    let m = lock();
    (0..m.nframes).filter(|&i| m.owner[i] == owner).count()
}

/// Free every frame owned by `owner`; returns how many were reclaimed.
pub fn free_owned(owner: u8) -> usize {
    let mut m = lock();
    let mut n = 0;
    for i in 0..m.nframes {
        if m.owner[i] == owner {
            m.release(i);
            n += 1;
        }
    }
    n
}

/// `n` physically contiguous unmovable frames (DMA-style), lowest address first.
pub fn alloc_contig(n: usize, owner: u8) -> Option<usize> {
    let start = {
        let mut m = lock();
        let start = metal_compact::find_run(&*m, m.low_hint, n)?;
        for i in start..start + n {
            m.claim(i, owner, FKind::Pinned);
        }
        // (the invariant is "no free frame below `low_hint`": the run may have been found beyond free frames that are
        // still there, so look for the first free one instead of jumping past the run)
        let mut h = m.low_hint;
        while h < m.nframes && m.kind[h] != FKind::Free {
            h += 1;
        }
        m.low_hint = h;
        start
    };
    for i in start..start + n {
        zero(RAM_BASE + i * PAGE);
    }
    Some(RAM_BASE + start * PAGE)
}

pub fn free_frames() -> usize {
    lock().free
}

pub fn total_frames() -> usize {
    lock().nframes
}

pub fn largest_free_run() -> usize {
    metal_compact::largest_free_run(&*lock())
}

pub fn free_runs() -> usize {
    metal_compact::free_runs(&*lock())
}
