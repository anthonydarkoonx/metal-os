//! Kernel memory objects that capabilities can name: plain shared RAM, DMA
//! buffers, and device MMIO windows. Includes the boot-time PCI probe for the virtio-blk device.
#![forbid(unsafe_code)]
use crate::sync::{SpinLock, R_OBJ};
use crate::mem;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjKind {
    Unused,
    Ram,
    Dma,
    Mmio,
    /// Created by a program (`SHM_CREATE`): single frames named by a list, shared by capability; see `shm_alloc`.
    Shared,
}

#[derive(Clone, Copy)]
pub struct MemObj {
    pub kind: ObjKind,
    pub pa: usize,
    pub pages: usize,
    /// MMIO: offset of the device inside its page. Shared memory: the program that created it (and is charged for it).
    pub aux: usize,
    /// Shared memory: the capability node of its creator's capability. The object lives exactly as long as that one.
    pub root: crate::cap::NodeId,
    /// Shared memory: where the list of its frames is (`ceil(pages / 1024)` kernel pages of `u32` frame numbers).
    pub list: usize,
}

pub const OBJ_SHARED: u8 = 0;
pub const OBJ_BLK_MMIO: u8 = 1;
pub const OBJ_BLK_RING: u8 = 2;
/// One DMA buffer per block-driver client; the driver picks by kernel-provided sender id.
pub const OBJ_BLK_BUF_ROFS: u8 = 3;
pub const OBJ_BLK_BUF_LFS: u8 = 4;
pub const OBJ_BLK_BUF_CLIENT: u8 = 5;
/// Pages shared between the test client and each filesystem server.
pub const OBJ_SHM_ROFS: u8 = 6;
pub const OBJ_SHM_LFS: u8 = 7;
/// Pages the VFS server shares with the storage servers and with each of its clients.
pub const OBJ_SHM_ROFS_VFS: u8 = 8;
pub const OBJ_SHM_LFS_VFS: u8 = 9;
pub const OBJ_SHM_VFS_CLIENT: u8 = 10;
pub const OBJ_SHM_VFS_PROGRAM: u8 = 11;
/// Result page shared by the benchmark clients and the conductor.
pub const OBJ_SHM_BENCH: u8 = 12;
/// Read-only copy of the block device's PCI configuration space (the driver parses its capabilities).
pub const OBJ_BLK_PCICFG: u8 = 13;
/// The display device: registers, config copy, control-queue page, command page, framebuffer.
pub const OBJ_GPU_MMIO: u8 = 14;
pub const OBJ_GPU_PCICFG: u8 = 15;
pub const OBJ_GPU_QUEUE: u8 = 16;
pub const OBJ_GPU_CMD: u8 = 17;
pub const OBJ_GPU_FB: u8 = 18;
/// 1024 x 768 pixels of 4 bytes, physically contiguous.
pub const FB_PAGES: usize = 768;
/// The keyboard: registers, config copy, and one page holding its event queue and buffers.
pub const OBJ_KBD_MMIO: u8 = 19;
pub const OBJ_KBD_PCICFG: u8 = 20;
pub const OBJ_KBD_QUEUE: u8 = 21;
/// Page the VFS server shares with the shell.
pub const OBJ_SHM_VFS_SHELL: u8 = 22;
/// Flag page the test conductor shares with the CPU-burner tasks (scheduler tests).
pub const OBJ_SHM_SPIN: u8 = 23;
/// Sessions of programs with file-system access: eight pages the VFS server has mapped (24..32).
pub const OBJ_SHM_VFS_PROG0: u8 = 24;
/// The network device: registers, config copy, the two virtqueue pages, the packet buffers (shared with `netd`), the
/// ring the driver and `netd` talk through, and the session pages of programs with network access (40..48).
pub const OBJ_NET_MMIO: u8 = 32;
pub const OBJ_NET_PCICFG: u8 = 33;
pub const OBJ_NET_QUEUES: u8 = 34;
pub const OBJ_NET_BUFS: u8 = 35;
pub const OBJ_NET_RING: u8 = 36;
pub const OBJ_SHM_NET_PROG0: u8 = 40;
/// The pointer (a second virtio-input device, a tablet): registers, config copy, queue page.
pub const OBJ_PTR_MMIO: u8 = 48;
pub const OBJ_PTR_PCICFG: u8 = 49;
pub const OBJ_PTR_QUEUE: u8 = 50;
/// The page the kernel's own virtio-rng exchange uses (ring and buffer); only exists while the device is being read.
pub const OBJ_RNG_DMA: u8 = 51;
/// 64 receive and 32 transmit buffers of 2 KiB.
pub const NET_BUF_PAGES: usize = 48;
pub const IRQ_BLK: u8 = 0;
pub const IRQ_GPU: u8 = 1;
pub const IRQ_NET: u8 = 2;

/// (ids are `u8`; the first 56 are the boot-time objects, the rest are created at run time)
const NOBJ: usize = 256;
static OBJS: SpinLock<[MemObj; NOBJ]> =
    SpinLock::new([MemObj { kind: ObjKind::Unused, pa: 0, pages: 0, aux: 0, root: 0, list: 0 }; NOBJ]);

/// All streams a driver role's devices sit behind (the input driver has two: keyboard and pointer).
pub fn sids_of_role(role: u8) -> [Option<u16>; 2] {
    crate::devices::sids_of_role(role)
}

pub fn get(i: u8) -> Option<MemObj> {
    OBJS.lock(R_OBJ).get(i as usize).copied().filter(|o| o.kind != ObjKind::Unused)
}

pub(crate) fn set(i: u8, kind: ObjKind, pa: usize, pages: usize, aux: usize) {
    OBJS.lock(R_OBJ)[i as usize] = MemObj { kind, pa, pages, aux, root: 0, list: 0 };
}

/// The page the kernel reads the virtio-rng device through (see `virtio_rng.rs`).
pub fn set_rng_dma(pa: usize) {
    set(OBJ_RNG_DMA, ObjKind::Dma, pa, 1, 0);
}

pub fn clear_rng_dma() {
    OBJS.lock(R_OBJ)[OBJ_RNG_DMA as usize].kind = ObjKind::Unused;
}

pub fn init() {
    set(OBJ_SHARED, ObjKind::Ram, mem::alloc(mem::OWNER_KERNEL).expect("shared page"), 1, 0);
    crate::platform::probe_devices();
}

/// First object id handed out dynamically (below it: the fixed boot objects).
/// Object numbers 52..=63 belong to the platform (its own devices: `platform.rs` names them); the objects programs make start here.
pub const FIRST_PLATFORM_OBJ: u8 = 52;
const FIRST_DYNAMIC: usize = 64;

/// Register a contiguous DMA-style object created at run time. `root` is the node of the creator's capability: the object
/// lives as long as that capability does (a copy of the capability, such as a thread's, never ends it).
pub fn alloc_dynamic(pa: usize, pages: usize, root: crate::cap::NodeId) -> Option<u8> {
    let mut t = OBJS.lock(R_OBJ);
    let i = (FIRST_DYNAMIC..NOBJ).find(|&i| t[i].kind == ObjKind::Unused)?;
    t[i] = MemObj { kind: ObjKind::Dma, pa, pages, aux: 0, root, list: 0 };
    Some(i as u8)
}

/// Dynamic objects in use (a leak check).
pub fn dynamic_in_use() -> usize {
    OBJS.lock(R_OBJ)[FIRST_DYNAMIC..].iter().filter(|o| o.kind != ObjKind::Unused).count()
}

pub fn is_dynamic(i: u8) -> bool {
    i as usize >= FIRST_DYNAMIC
}

pub fn release(i: u8) {
    set(i, ObjKind::Unused, 0, 0, 0);
}

/// A page the kernel allocates only so that an object exists where another machine has a device window.
pub(crate) fn dummy_page() -> usize {
    mem::alloc(mem::OWNER_KERNEL).expect("dummy page")
}

pub fn blk_present() -> bool {
    get(OBJ_BLK_MMIO).is_some()
}

pub fn gpu_present() -> bool {
    get(OBJ_GPU_MMIO).is_some()
}

pub fn net_present() -> bool {
    get(OBJ_NET_MMIO).is_some()
}

#[allow(dead_code)] // (the device table asks `devices::present` now; a platform may still call this)
pub fn kbd_present() -> bool {
    get(OBJ_KBD_MMIO).is_some()
}

// ---- memory that programs create and share (SHM_CREATE) ------------------------------------------------------------------
//
// A shared object is made of single frames (so it never needs a contiguous run), pinned (never moved or swapped, so that
// every holder sees the same physical page and `WAIT_WORD` can key words by address). The frames are owned by the creating
// program like its other memory. The creator's own capability is the object's kill switch: revoking it with `CAP_REVOKE`
// flag 1 frees the object and unmaps it everywhere. When the creator merely exits, holders keep what they have (an
// *orphan*, see `shm_orphan`): the memory goes when the last of them lets go.

/// Most pages one shared object may have (16 MiB).
pub const SHM_MAX_PAGES: usize = 4096;
const PER_LIST_PAGE: usize = mem::PAGE / 4;

/// Frames for a new shared object of `pages` pages owned by `owner`, and the list that names them (its address).
pub fn shm_alloc(pages: usize, owner: u8) -> Option<usize> {
    let lists = (pages + PER_LIST_PAGE - 1) / PER_LIST_PAGE;
    let list = mem::alloc_contig(lists, mem::OWNER_KERNEL)?;
    for i in 0..pages {
        match mem::alloc_kind(owner, mem::FKind::Pinned) {
            Some(pa) => crate::frame::phys::write_u32(list + 4 * i, (pa >> 12) as u32),
            None => {
                shm_release_frames(list, i, pages);
                return None;
            }
        }
    }
    Some(list)
}

/// Give back the first `have` frames named by `list` and the list itself (made for an object of `pages` pages).
pub fn shm_release_frames(list: usize, have: usize, pages: usize) {
    for j in 0..have {
        mem::free(crate::frame::phys::read_u32(list + 4 * j) as usize * mem::PAGE);
    }
    for l in 0..(pages + PER_LIST_PAGE - 1) / PER_LIST_PAGE {
        mem::free(list + l * mem::PAGE);
    }
}

/// The physical address of page `p` of a shared object.
pub fn shm_frame(o: &MemObj, p: usize) -> usize {
    debug_assert!(o.kind == ObjKind::Shared && p < o.pages);
    crate::frame::phys::read_u32(o.list + 4 * p) as usize * mem::PAGE
}

/// Register a shared object whose frames `shm_alloc` returned.
pub fn shm_register(pages: usize, creator: usize, root: crate::cap::NodeId, list: usize) -> Option<u8> {
    let mut t = OBJS.lock(R_OBJ);
    let i = (FIRST_DYNAMIC..NOBJ).find(|&i| t[i].kind == ObjKind::Unused)?;
    t[i] = MemObj { kind: ObjKind::Shared, pa: 0, pages, aux: creator, root, list };
    Some(i as u8)
}

/// `MemObj::aux` of a shared object whose creator is gone.
pub const SHM_ORPHAN: usize = usize::MAX;
/// How many shared objects are orphans (the sweep after a revocation looks for them only when there are some).
static ORPHANS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The creator's capability goes while others still hold the object: it lives on, belonging to the kernel and charged to
/// nobody, until the last holder lets go of it (`shm_destroy` then). Whoever maps it keeps what they see; nothing faults.
pub fn shm_orphan(i: u8) {
    let mut t = OBJS.lock(R_OBJ);
    let o = &mut t[i as usize];
    if o.kind != ObjKind::Shared || o.aux == SHM_ORPHAN {
        return;
    }
    for p in 0..o.pages {
        mem::set_owner(crate::frame::phys::read_u32(o.list + 4 * p) as usize * mem::PAGE, mem::OWNER_KERNEL);
    }
    crate::spawn::release_contig(o.aux, o.pages);
    o.aux = SHM_ORPHAN;
    o.root = crate::cap::NO_PARENT;
    ORPHANS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

/// Is this a shared object whose creator is gone?
pub fn shm_is_orphan(i: u8) -> bool {
    get(i).is_some_and(|o| o.kind == ObjKind::Shared && o.aux == SHM_ORPHAN)
}

/// The ids of the orphaned shared objects, into `out`; how many.
pub fn shm_orphans(out: &mut [u8; 16]) -> usize {
    if ORPHANS.load(core::sync::atomic::Ordering::SeqCst) == 0 {
        return 0;
    }
    let t = OBJS.lock(R_OBJ);
    let mut n = 0;
    for (i, o) in t.iter().enumerate().skip(FIRST_DYNAMIC) {
        if o.kind == ObjKind::Shared && o.aux == SHM_ORPHAN && n < out.len() {
            out[n] = i as u8;
            n += 1;
        }
    }
    n
}

/// Free a shared object: its frames, its list and its table entry, and the creator's charge for it. (The capabilities and
/// mappings must have been revoked first.)
pub fn shm_destroy(i: u8) {
    let Some(o) = get(i).filter(|o| o.kind == ObjKind::Shared) else { return };
    shm_release_frames(o.list, o.pages, o.pages);
    if o.aux == SHM_ORPHAN {
        ORPHANS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    } else {
        crate::spawn::release_contig(o.aux, o.pages);
    }
    release(i);
}
