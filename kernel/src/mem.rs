//! Physical frame allocator over per-frame tables carved out of the top of RAM at boot (sized from the RAM it finds), behind one lock.
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
pub const PAGE: usize = 4096;
/// Owner numbers in use: nobody, the kernel, and every task slot (plus a few the self-test borrows).
const OWNERS: usize = OWNER_TASK0 as usize + crate::task::MAX_TASKS + 8;
/// End of a per-owner list.
const NIL: u32 = u32::MAX;

/// The RAM the kernel uses out of `ram` bytes: beyond the first GiB the direct map works in 2 MiB blocks, so the size is rounded down to them.
pub const fn usable_ram(ram: usize) -> usize {
    if ram > crate::frame::phys::GIB { ram & !((2 << 20) - 1) } else { ram }
}

/// Bytes of per-frame tables (owner 2 + kind 1 + reverse map 8 + two list links of 4) for `n` frames, rounded up to whole pages. They sit in the last frames of RAM.
pub const fn meta_bytes(n: usize) -> usize {
    (n * 19).div_ceil(PAGE) * PAGE
}

/// Who holds a frame: nobody, the kernel, or task `tid` as `OWNER_TASK0 + tid`.
pub type Owner = u16;
pub const OWNER_FREE: Owner = 0;
pub const OWNER_KERNEL: Owner = 1;
/// Task `tid` owns its frames as `OWNER_TASK0 + tid`.
pub const OWNER_TASK0: Owner = 2;

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
    owner: &'static mut [Owner],
    kind: &'static mut [FKind],
    /// For movable frames: `tid << 32 | (virtual address >> 12)`. (The page number of an address in the 39-bit user space takes 27 bits, more
    /// than the 24 that fit beside a task number in 32: a position-independent image is placed as high as 256 GiB.)
    rmap: &'static mut [u64],
    /// Per-owner lists of the frames each owner holds (not for `OWNER_FREE`): a doubly linked list through these, so that a task's
    /// frames are found, counted and freed without scanning all of RAM.
    next: &'static mut [u32],
    prev: &'static mut [u32],
    head: [u32; OWNERS],
    held: [u32; OWNERS],
    nframes: usize,
    /// No free frame exists below `low_hint`, nor above `high_hint`.
    low_hint: usize,
    high_hint: usize,
    free: usize,
}

static MEM: SpinLock<MemState> = SpinLock::new(MemState {
    owner: &mut [],
    kind: &mut [],
    rmap: &mut [],
    next: &mut [],
    prev: &mut [],
    head: [NIL; OWNERS],
    held: [0; OWNERS],
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
    /// Put frame `i` on `owner`'s list (unless the owner is nobody).
    fn link(&mut self, i: usize, owner: Owner) {
        self.owner[i] = owner;
        if owner == OWNER_FREE {
            return;
        }
        let h = self.head[owner as usize];
        (self.next[i], self.prev[i]) = (h, NIL);
        if h != NIL {
            self.prev[h as usize] = i as u32;
        }
        self.head[owner as usize] = i as u32;
        self.held[owner as usize] += 1;
    }

    /// Take frame `i` off its owner's list and make it nobody's.
    fn unlink(&mut self, i: usize) {
        let owner = self.owner[i];
        self.owner[i] = OWNER_FREE;
        if owner == OWNER_FREE {
            return;
        }
        let (p, n) = (self.prev[i], self.next[i]);
        if p == NIL {
            self.head[owner as usize] = n;
        } else {
            self.next[p as usize] = n;
        }
        if n != NIL {
            self.prev[n as usize] = p;
        }
        self.held[owner as usize] -= 1;
    }

    fn claim(&mut self, i: usize, owner: Owner, kind: FKind) -> usize {
        self.link(i, owner);
        self.kind[i] = kind;
        self.free -= 1;
        RAM_BASE + i * PAGE
    }

    fn release(&mut self, i: usize) {
        self.unlink(i);
        self.kind[i] = FKind::Free;
        self.free += 1;
        self.low_hint = self.low_hint.min(i);
        self.high_hint = self.high_hint.max(i);
    }

    /// Every list is well formed and accounts for exactly the frames that have an owner.
    fn lists_intact(&self) -> bool {
        let mut total = 0usize;
        for o in 1..OWNERS {
            let (mut n, mut prev, mut i) = (0u32, NIL, self.head[o]);
            while i != NIL {
                let k = i as usize;
                if k >= self.nframes || self.owner[k] as usize != o || self.prev[k] != prev || n > self.nframes as u32 {
                    return false;
                }
                (n, prev, i) = (n + 1, i, self.next[k]);
            }
            if n != self.held[o] {
                return false;
            }
            total += n as usize;
        }
        total + self.free == self.nframes
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
        self.link(dst, self.owner[src]);
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
    let n = ram_size / PAGE;
    crate::frame::phys::set_ram_size(n * PAGE);
    // the tables take the last frames of RAM (`kaslr` keeps the kernel image clear of them)
    let meta = n - meta_bytes(n) / PAGE;
    assert!(kernel.end <= RAM_BASE + meta * PAGE, "the kernel image overlaps the frame tables");
    let t = crate::frame::phys::meta_tables(RAM_BASE + meta * PAGE, n);
    (m.owner, m.kind, m.rmap, m.next, m.prev) = (t.owner, t.kind, t.rmap, t.next, t.prev);
    let low = (crate::kaslr::LINK_BASE - RAM_BASE) / PAGE;
    let (k0, k1) = ((kernel.start - RAM_BASE) / PAGE, (kernel.end - RAM_BASE).div_ceil(PAGE));
    let mut free = 0;
    for i in 0..n {
        let kernel = i < low || i >= meta || (k0..k1).contains(&i);
        m.link(i, if kernel { OWNER_KERNEL } else { OWNER_FREE });
        m.kind[i] = if kernel { FKind::Kernel } else { FKind::Free };
        free += !kernel as usize;
    }
    m.nframes = n;
    m.low_hint = (0..n).find(|&i| m.kind[i] == FKind::Free).unwrap_or(n);
    m.high_hint = meta.saturating_sub(1);
    m.free = free;
}

fn zero(pa: usize) {
    crate::frame::phys::zero_page(pa);
}

/// Allocate one zeroed *unmovable* frame from the bottom of RAM.
pub fn alloc_kind(owner: Owner, kind: FKind) -> Option<usize> {
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

/// Allocate an unmovable kernel-owned frame that lies in the identity-mapped window, for the code that reaches a frame through its
/// physical address used as a pointer (the SMMU's tables and queues, a probe's DMA page). Frames come from the bottom of RAM, so this
/// fails only when the whole first GiB is taken by unmovable memory.
pub fn alloc_identity(owner: Owner) -> Option<usize> {
    let pa = alloc_kind(owner, FKind::Kernel)?;
    if pa < crate::frame::phys::ID_END {
        return Some(pa);
    }
    free(pa);
    None
}

/// Allocate an unmovable kernel-owned frame.
pub fn alloc(owner: Owner) -> Option<usize> {
    alloc_kind(owner, FKind::Kernel)
}

/// Allocate one zeroed *movable* frame (from the top of RAM) that will be mapped at `va` in `tid`.
pub fn alloc_movable(owner: Owner, tid: usize, va: usize) -> Option<usize> {
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

/// Boot-time check of the per-owner lists: counts follow allocation, release, re-owning and `free_owned`, and every list is intact
/// (each frame on a list names its owner, links agree both ways, the lengths match the counts, held + free = all frames).
pub fn owner_lists_selftest() -> bool {
    const O1: Owner = OWNERS as Owner - 2;
    const O2: Owner = OWNERS as Owner - 1;
    let mut ok = count_owned(O1) == 0 && count_owned(O2) == 0;
    let (a, b, c) = (alloc(O1), alloc_movable(O1, 0, 0), alloc_kind(O1, FKind::Pinned));
    let (Some(a), Some(b), Some(c)) = (a, b, c) else { return false };
    ok &= count_owned(O1) == 3 && owner_of(b) == O1;
    free(b); // (the middle of the list)
    ok &= count_owned(O1) == 2 && owner_of(b) == OWNER_FREE;
    set_owner(a, O2);
    ok &= count_owned(O1) == 1 && count_owned(O2) == 1 && owner_of(a) == O2 && owner_of(c) == O1;
    ok &= free_owned(O1) == 1 && free_owned(O2) == 1 && count_owned(O1) == 0 && count_owned(O2) == 0;
    ok &= lock().lists_intact();
    ok
}

/// Boot-time check of the direct map: frames above the identity window are zeroed when allocated, hold what is written, copy to each
/// other, and are found by the checked accessors. `None` if the machine's RAM does not reach past the window.
pub fn highmem_selftest() -> Option<bool> {
    use crate::frame::phys;
    let end = RAM_BASE + lock().nframes * PAGE;
    if end <= phys::ID_END {
        return None;
    }
    // (movable frames come from the top of RAM, so they are above the window as soon as RAM reaches past it)
    let (Some(a), Some(b)) = (alloc_movable(OWNER_KERNEL, 0, 0), alloc_movable(OWNER_KERNEL, 0, 0)) else { return Some(false) };
    let mut ok = a >= phys::ID_END && b >= phys::ID_END && phys::is_ram_page(a) && phys::va(a) == phys::DIRECT_BASE + a;
    ok &= (0..PAGE / 8).all(|i| phys::read_u64(a + 8 * i) == 0);
    for i in 0..PAGE / 8 {
        phys::write_u64(a + 8 * i, (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ a as u64);
    }
    phys::copy_page(b, a);
    ok &= (0..PAGE / 8).all(|i| phys::read_u64(b + 8 * i) == (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ a as u64);
    let mut buf = [0u8; 64];
    ok &= phys::try_copy_from(b + 100, &mut buf) && phys::try_copy_to(a + 100, &buf);
    free(a);
    free(b);
    Some(ok)
}

/// Is the frame at `pa` a private, relocatable user page (as opposed to shared or device memory, which never moves)?
pub fn is_movable(pa: usize) -> bool {
    // (an address outside RAM, a device page for one, is no frame at all, and never movable)
    let m = lock();
    pa.checked_sub(RAM_BASE).map_or(false, |d| m.kind.get(d / PAGE).map_or(false, |&k| k == FKind::Movable))
}

/// Who owns the frame at `pa` (`OWNER_FREE` if nobody).
pub fn owner_of(pa: usize) -> Owner {
    // (outside RAM there is no frame, so nobody owns it)
    let m = lock();
    pa.checked_sub(RAM_BASE).and_then(|d| m.owner.get(d / PAGE).copied()).unwrap_or(OWNER_FREE)
}

/// Hand a frame to another owner (shared memory that outlives the program that made it goes to the kernel).
pub fn set_owner(pa: usize, owner: Owner) {
    let mut m = lock();
    let i = (pa - RAM_BASE) / PAGE;
    assert!(m.kind[i] != FKind::Free, "owner of a free frame");
    m.unlink(i);
    m.link(i, owner);
}

/// Free a single frame.
pub fn free(pa: usize) {
    let mut m = lock();
    let i = (pa - RAM_BASE) / PAGE;
    assert!(m.kind[i] != FKind::Free, "double free");
    m.release(i);
}

/// How many frames `owner` holds.
pub fn count_owned(owner: Owner) -> usize {
    lock().held[owner as usize] as usize
}

/// Free every frame owned by `owner`; returns how many were reclaimed.
pub fn free_owned(owner: Owner) -> usize {
    let mut m = lock();
    let mut n = 0;
    while m.head[owner as usize] != NIL {
        let i = m.head[owner as usize] as usize;
        m.release(i);
        n += 1;
    }
    n
}

/// `n` physically contiguous unmovable frames (DMA-style), lowest address first.
pub fn alloc_contig(n: usize, owner: Owner) -> Option<usize> {
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
