//! Tasks: address space, capability table, lifecycle (spawn / die / restart).
//!
//! Per-task state is split by who may touch it:
//!  * `Task` (below) sits behind the task's own lock: capabilities, mappings, page tables,
//!    reply links, the async-call table, heap size.
//!  * scheduling state (run state, running flag, dispatch stamps) lives in `sched.rs`
//!    behind the scheduler lock;
//!  * `Info` is written once at spawn and read-only afterwards, so it needs no lock;
//!  * the saved register frame is handed over by protocol, see `frame()`.
#![forbid(unsafe_code)]
use crate::cap::*;
use crate::ipc::{self, Endpoint, NEP};
use crate::mem::{self, PAGE};
use crate::sched;
use crate::sync::{Guard, Slots, SpinLock, R_TASK};
use crate::trap::TrapFrame;
use crate::{kprintln, mmu, obj};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use metal_sched::Class;

/// Boot-time tasks first, then slots for tasks created at run time (programs and their threads). At most 64: the
/// scheduler keeps per-task run-state bits in 64-bit masks, and lock ranks (sync.rs) leave room for 70 task locks.
pub const MAX_TASKS: usize = 64;
/// First slot that may hold a task created at run time.
pub const FIRST_DYNAMIC_TASK: usize = 27;
pub const USER_BASE: usize = 0x8000_0000;
/// Code and data of a program lie between `USER_BASE` and here (the loader's limit; the old stack position is kept out).
pub const IMAGE_END: usize = 0x800F_0000;
/// The stack is `STACK_PAGES` pages whose top is chosen at random inside this region (`random_stack_top`); the page
/// below the stack and the page at its top stay unmapped (guard pages: nothing maps into this region but the stack).
pub const STACK_REGION_LO: usize = 0x8040_0000;
pub const STACK_REGION_HI: usize = 0x8100_0000;
pub const STACK_PAGES: usize = 16;
/// Anonymous per-task heap window, grown on demand with HEAP_GROW. It starts a random number of pages (below
/// `HEAP_SLIDE_PAGES`) after `HEAP_BASE` and ends at `HEAP_END`, which leaves the page after the last heap page unmapped.
pub const HEAP_BASE: usize = 0x8100_0000;
pub const HEAP_SLIDE_PAGES: usize = 4096;
pub const HEAP_END: usize = 0x8F00_0000;
pub const MAX_HEAP_PAGES: usize = 512;
/// The storage servers (roles 6 and 7: the log-structured store and the file layer) stage a whole write in memory before it is committed, so the
/// largest file that can be installed is about what their heap holds: 2048 pages (8 MiB) takes a 2.6 MB package (512 pages took 1.3 MB).
pub const STORAGE_HEAP_PAGES: usize = 2048;
/// The desktop (role 13): its own heap and the window surfaces of its clients (about 47 MiB in all: the wallpaper and its frosted copy, the shade snapshot, caches, and up to four 3 MiB fullscreen surfaces; a cap, not a reservation).
pub const DESKTOP_HEAP_PAGES: usize = crate::platform::DESKTOP_HEAP_PAGES;
/// Where this program's stack starts: a random page-aligned top with `STACK_PAGES` pages below it, an unmapped page
/// below those and an unmapped page at the top and above.
pub fn random_stack_top() -> usize {
    let lo = STACK_REGION_LO + (STACK_PAGES + 1) * PAGE;
    let hi = STACK_REGION_HI - PAGE;
    lo + crate::rand::below((hi - lo) / PAGE + 1) * PAGE
}

pub fn random_heap_base() -> usize {
    HEAP_BASE + crate::rand::below(HEAP_SLIDE_PAGES) * PAGE
}

/// Benchmark task layout (spawn order in main.rs).
pub const NOT_PINNED: u8 = u8::MAX;
pub const FIRST_BENCH_SERVER: usize = 10;
pub const FIRST_BENCH_CLIENT: usize = 14;
/// The test conductor may hold far more (it exercises memory pressure).
pub const CONDUCTOR_HEAP_PAGES: usize = 60_000;
/// What a boot-time task may hand down to the programs it starts: the conductor (which runs the test suites) a good deal
/// of the machine, the others (shell, the programs task, the fuzzer) 32 MiB.
pub const CONDUCTOR_BUDGET_PAGES: usize = 30_000;
pub const ROLE_BUDGET_PAGES: usize = 8_192;
/// Where a task's submission/completion ring page is mapped (see ring.rs).
/// (the page is put at a random page of the 16 MiB from here, `RING_SLIDE_PAGES`)
pub const RING_VA: usize = 0x8F00_0000;
pub const RING_SLIDE_PAGES: usize = 4096;
/// Where the image of a position-independent program is put (`spawn::FLAG_PIE`): a random page in here, which is most of
/// the 39-bit address space (L1 entries 8..256: below the ECAM window, above the 4 GiB the SoC's devices may use).
pub const PIE_LO: usize = 0x2_0000_0000;
pub const PIE_HI: usize = 0x40_0000_0000;
/// The largest position-independent image: 8 MiB (a fixed one has to fit below the first mappings: `IMAGE_END`).
pub const PIE_MAX_IMAGE: usize = 8 << 20;
pub const SHM_LO: usize = 0x8020_0000;
pub const SHM_HI: usize = 0x8040_0000;
/// A second, large window for shared memory (512 MiB): for programs that map many big buffers.
pub const BIG_SHM_LO: usize = 0xA000_0000;
pub const BIG_SHM_HI: usize = 0xC000_0000;
pub const MAX_MAPS: usize = 64;
/// A task that keeps crashing is left dead rather than restarted forever.
/// A service may be restarted this many times in a row ...
const MAX_RESTARTS: u32 = 5;
/// ... but the count starts over once an instance has run this long (a driver that crashes once in a
/// while must not use up its budget for good; one that crashes in a loop still gives up).
const STABLE_TICKS: u64 = 300;

static USER_IMG: &[u8] = include_bytes!("../../user/build/user.elf");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Free,
    Ready,
    RecvWait(u8),
    CallWait,
    Sleep(u64),
    IrqWait(u8),
    /// Sleeping in RING_ENTER until this many completions are available.
    RingWait(u32),
    /// Blocked in WAIT for this child program to exit (spawn.rs).
    ChildWait(u8),
    /// Blocked in WAIT_WORD (thread.rs) until woken, or until the counter reaches this value (0 = no deadline).
    WordWait(u64),
    /// A program created by SPAWN whose code is still being loaded by its parent (spawn.rs); never scheduled.
    Embryo,
    Dead,
}

#[derive(Clone, Copy)]
pub struct MapRec {
    pub used: bool,
    pub va: usize,
    pub pages: usize,
    pub node: crate::cap::NodeId,
}


/// Everything about a task that other cores may change: behind the task's lock.
pub struct Task {
    pub l1: usize,
    pub caps: [Cap; NCAPS],
    pub maps: [MapRec; MAX_MAPS],
    pub heap_pages: usize,
    /// Thread stacks the kernel made (`THREAD_CREATE` with no stack): (lowest mapped address, pages, thread); pages are
    /// counted against the heap limit.
    pub tstacks: [(usize, usize, u8); crate::spawn::MAX_THREADS],
    /// This address space's randomised layout (see `random_stack_top`, `random_heap_base`); set when the space is built.
    pub stack_top: usize,
    pub heap_base: usize,
    pub ring_pa: usize,
    pub pending: [crate::ring::Pending; crate::ring::MAX_PENDING],
    /// (caller, async tag or 0) we still owe a reply to.
    pub reply_to: Option<(u8, u8)>,
    /// Server currently handling our outstanding call.
    pub callee: Option<u8>,
}

impl Task {
    /// Pages held against the program's heap limit (contiguous allocations are counted separately).
    pub fn held_pages(&self) -> usize {
        self.heap_pages + self.tstacks.iter().map(|s| s.1).sum::<usize>()
    }

    const EMPTY: Task = Task {
        l1: 0,
        caps: [Cap::NONE; NCAPS],
        maps: [MapRec { used: false, va: 0, pages: 0, node: 0 }; MAX_MAPS],
        heap_pages: 0,
        tstacks: [(0, 0, 0); crate::spawn::MAX_THREADS],
        stack_top: 0,
        heap_base: 0,
        ring_pa: 0,
        pending: [crate::ring::Pending::FREE; crate::ring::MAX_PENDING],
        reply_to: None,
        callee: None,
    };
}

static TASKS: [SpinLock<Task>; MAX_TASKS] = [const { SpinLock::new(Task::EMPTY) }; MAX_TASKS];

/// Lock task `tid` (rank `R_TASK + tid`: when taking several, take them in ascending order).
pub fn lock(tid: usize) -> Guard<'static, Task> {
    TASKS[tid].lock(R_TASK + tid as u16)
}

/// A task's name: up to 15 bytes of printable ASCII, kept inside the `Info` so a copy of it stays valid when the slot is reused.
#[derive(Clone, Copy)]
pub struct Name {
    buf: [u8; 15],
    len: u8,
}

impl Name {
    pub const EMPTY: Name = Name { buf: [0; 15], len: 0 };

    /// From a string (cut at 15 bytes; anything not printable ASCII becomes `?`).
    pub fn new(s: &[u8]) -> Name {
        let mut n = Name::EMPTY;
        for &b in s.iter().take(15) {
            n.buf[n.len as usize] = if (0x20..0x7F).contains(&b) { b } else { b'?' };
            n.len += 1;
        }
        n
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("?") // (`new` keeps it ASCII)
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Facts about a task: set when the slot is filled (at boot, or by SPAWN while the slot is free) and, for the two limits, by QUOTA.
/// Read by copy (`info`), so no reference outlives the slot's use.
#[derive(Clone, Copy)]
pub struct Info {
    pub role: u8,
    pub restartable: bool,
    pub name: Name,
    pub class: Class,
    /// Core this task is confined to (`NOT_PINNED` = any).
    pub pinned: u8,
    /// Most pages this task may hold itself: heap plus contiguous allocations (`MC_ALLOC`).
    pub heap_quota: usize,
    /// Most pages this task may hand down to the programs it starts (their quotas, budgets and images; see spawn.rs).
    pub budget: usize,
}

static INFO: Slots<Info, MAX_TASKS> = Slots::new(
    [const { Info { role: 0, restartable: false, name: Name::EMPTY, class: Class::Background, pinned: NOT_PINNED, heap_quota: MAX_HEAP_PAGES, budget: 0 } }; MAX_TASKS],
);

pub fn info(tid: usize) -> Info {
    *INFO.read(tid)
}

static FRAMES: Slots<TrapFrame, MAX_TASKS> = Slots::new([TrapFrame::ZERO; MAX_TASKS]);

/// The task's saved registers. Not behind a lock: whoever currently *owns* the task writes it.
/// That is the core running it (from kernel entry until its dispatch), or, while it is blocked,
/// the one waker that completed what it waits for (the endpoint / interrupt / ring lock that
/// hands the task over guarantees there is exactly one). The scheduler lock, taken after the
/// last write and before the task becomes runnable, publishes the contents to the next core.
///
/// The access is checked: while the returned guard lives, a second `frame(tid)` (on this core or another) panics instead of
/// handing out a second `&mut`.
pub fn frame(tid: usize) -> crate::sync::RefMut<'static, TrapFrame> {
    FRAMES.write(tid)
}

/// Root of the task's page tables, readable by the dispatcher without taking the task lock.
static L1: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

pub fn l1_of(tid: usize) -> usize {
    L1[tid].load(Ordering::Acquire)
}

pub static RESTARTS: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// Restarts since the last stable period (what `MAX_RESTARTS` limits); `RESTARTS` stays cumulative.
static STREAK: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static LAST_RESTART: [core::sync::atomic::AtomicU64; MAX_TASKS] = [const { core::sync::atomic::AtomicU64::new(0) }; MAX_TASKS];
/// Timer ticks each task has run on each core.
pub static TICKS_ON: [[AtomicU32; crate::smp::MAX_CPUS]; MAX_TASKS] =
    [const { [const { AtomicU32::new(0) }; crate::smp::MAX_CPUS] }; MAX_TASKS];

pub fn owner(tid: usize) -> u8 {
    mem::OWNER_TASK0 + tid as u8
}

/// The task whose address space `tid` runs in: itself, unless it is a thread (thread.rs). Everything about memory — the
/// page tables, the mappings, the heap, the frames and who owns them — is kept under that task's lock and name.
static SPACE: [core::sync::atomic::AtomicU8; MAX_TASKS] = [const { core::sync::atomic::AtomicU8::new(u8::MAX) }; MAX_TASKS];

pub fn aspace(tid: usize) -> usize {
    match SPACE[tid].load(Ordering::Acquire) {
        u8::MAX => tid,
        leader => leader as usize,
    }
}

/// Make `tid` a thread of the program `leader` (a free slot), or (`None`) an ordinary task again.
pub fn set_aspace(tid: usize, leader: Option<usize>) {
    SPACE[tid].store(leader.map_or(u8::MAX, |l| l as u8), Ordering::Release);
}

/// The tasks (including `leader`) that run in `leader`'s address space and are not gone.
pub fn members(leader: usize) -> impl Iterator<Item = usize> {
    (0..MAX_TASKS).filter(move |&m| aspace(m) == leader && !matches!(sched::state(m), State::Free | State::Dead))
}

fn install(t: &mut Task, tid: usize, slot: usize, kind: Kind, obj: u8, rights: Rights) {
    let node = mint_node(None).expect("capability nodes exhausted");
    t.caps[slot] = Cap { kind, obj, rights, node };
    if kind == Kind::Endpoint && rights & R_RECV != 0 {
        sched::note_recv_endpoint(tid, obj);
    }
}

/// Initial capabilities per role: the kernel's static "system policy".
fn boot_caps(t: &mut Task, tid: usize, role: u8) {
    install(t, tid, crate::syscall::CONSOLE_SLOT, Kind::Console, 0, 0);
    // the driver tasks (roles 4, 12, 14, 18): what their devices' entries in the registry list
    crate::devices::grant_to_driver(role, |slot, kind, obj, rights| install(t, tid, slot, kind, obj, rights));
    match role {
        0 => {
            install(t, tid, 0, Kind::Endpoint, ipc::EP_SERVICE, R_SEND);
            install(t, tid, 1, Kind::Memory, 0, R_MAP | R_WRITE | R_GRANT);
            install(t, tid, 2, Kind::Endpoint, ipc::EP_DRIVER, R_SEND);
            if obj::blk_present() {
                // clients talk to the block driver and share its buffer (no INFO right: no physical addresses)
                install(t, tid, 3, Kind::Endpoint, ipc::EP_BLK, R_SEND);
                install(t, tid, 4, Kind::Memory, obj::OBJ_BLK_BUF_CLIENT, R_MAP | R_WRITE);
                install(t, tid, 5, Kind::Endpoint, ipc::EP_ROFS, R_SEND);
                install(t, tid, 6, Kind::Memory, obj::OBJ_SHM_ROFS, R_MAP | R_WRITE);
                install(t, tid, 7, Kind::Endpoint, ipc::EP_LFS, R_SEND);
                install(t, tid, 8, Kind::Memory, obj::OBJ_SHM_LFS, R_MAP | R_WRITE);
                install(t, tid, 9, Kind::Endpoint, ipc::EP_VFS, R_SEND);
                install(t, tid, 10, Kind::Memory, obj::OBJ_SHM_VFS_CLIENT, R_MAP | R_WRITE);
                install(t, tid, 11, Kind::MemCtl, 0, R_MEM_STATS | R_MEM_ALLOC | R_MEM_SWAP | R_MEM_PAGER | R_GRANT); // (grant: the memory-limit tests hand it to a program)
                // conductor may start each benchmark client and read their results
                for k in 0..ipc::BENCH_PAIRS {
                    install(t, tid, 16 + k, Kind::Endpoint, ipc::EP_BENCH_GO + k as u8, R_SEND);
                }
                install(t, tid, 20, Kind::Memory, obj::OBJ_SHM_BENCH, R_MAP);
            }
            if obj::blk_present() {
                // scheduler tests: start and stop the CPU burners, and reserve real-time shares
                install(t, tid, 24, Kind::Memory, obj::OBJ_SHM_SPIN, R_MAP | R_WRITE);
                install(t, tid, 25, Kind::Sched, 0, R_RT);
                // programs: the right to create them (and to pass it on), and a demo endpoint they may be given
                install(t, tid, 26, Kind::Spawn, 0, R_SPAWN | R_SPAWN_FS | R_SPAWN_NET | R_GRANT);
                install(t, tid, 12, Kind::Factory, 0, R_EP_CREATE | R_GRANT);
                install(t, tid, 27, Kind::Endpoint, ipc::EP_DRIVER, R_SEND | R_GRANT);
                install(t, tid, 28, Kind::Admin, 0, R_REVOKE | R_GRANT | R_POWER); // (the desktop session hands a copy to the program manager for the task manager, and one with only `R_POWER` to the desktop)
            }
            if crate::platform::chaos_seed() != 0 {
                // chaos mode: the conductor drives a workload against every service and starts the monkey
                install(t, tid, 14, Kind::Endpoint, ipc::EP_KBD, R_SEND);
                install(t, tid, 22, Kind::Endpoint, ipc::EP_MONKEY, R_SEND);
            }
            if obj::gpu_present() {
                // the conductor talks to the display driver and to the screen task; it never holds the framebuffer
                install(t, tid, 21, Kind::Endpoint, ipc::EP_GPU, R_SEND);
                install(t, tid, 23, Kind::Endpoint, ipc::EP_INFO, R_SEND);
            }
        }
        1 => install(t, tid, 0, Kind::Endpoint, ipc::EP_SERVICE, R_RECV),
        2 => install(t, tid, 0, Kind::Endpoint, ipc::EP_DRIVER, R_RECV),
        9 => install(t, tid, 0, Kind::MemCtl, 0, R_MEM_STATS | R_MEM_COMPACT | R_MEM_SWAP), // compactd
        10 => {
            // benchmark server `k = tid - FIRST_BENCH_SERVER`: serves its own endpoint, nothing else
            install(t, tid, 0, Kind::Endpoint, ipc::EP_BENCH_SRV + (tid - FIRST_BENCH_SERVER) as u8, R_RECV);
        }
        11 => {
            // benchmark client: calls its server, waits for a start message, reports in a shared page
            let k = (tid - FIRST_BENCH_CLIENT) as u8;
            install(t, tid, 0, Kind::Endpoint, ipc::EP_BENCH_SRV + k, R_SEND);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_BENCH_GO + k, R_RECV);
            install(t, tid, 2, Kind::Memory, obj::OBJ_SHM_BENCH, R_MAP | R_WRITE);
        }
        16 => {
            // CPU burner for the scheduler tests: reads the conductor's go/stop flag, nothing else
            install(t, tid, 0, Kind::Memory, obj::OBJ_SHM_SPIN, R_MAP);
        }
        17 => {
            // the syscall fuzzer (user/src/fuzz.rs): a deliberately mixed bag, one grantable capability
            install(t, tid, 0, Kind::Endpoint, ipc::EP_SERVICE, R_SEND | R_GRANT);
            // (slot 1 stays empty: the demo driver crashes on demand, which is not a finding)
            install(t, tid, 2, Kind::Endpoint, ipc::EP_VFS, R_SEND);
            install(t, tid, 3, Kind::Memory, obj::OBJ_SHM_VFS_PROGRAM, R_MAP | R_WRITE);
            install(t, tid, 4, Kind::Endpoint, ipc::EP_BLK, R_SEND);
            install(t, tid, 5, Kind::MemCtl, 0, R_MEM_STATS | R_MEM_ALLOC | R_MEM_SWAP | R_MEM_COMPACT);
            install(t, tid, 6, Kind::Sched, 0, R_RT);
            install(t, tid, 7, Kind::Memory, obj::OBJ_SHM_SPIN, R_MAP);
            install(t, tid, 8, Kind::Endpoint, ipc::EP_KBD, R_SEND);
            install(t, tid, 9, Kind::Endpoint, ipc::EP_GPU, R_SEND);
            install(t, tid, 10, Kind::Endpoint, ipc::EP_ROFS, R_SEND);
            install(t, tid, 11, Kind::Endpoint, ipc::EP_LFS, R_SEND);
            install(t, tid, 12, Kind::Spawn, 0, R_SPAWN | R_SPAWN_FS | R_GRANT); // children with random images; they must die with it
            install(t, tid, 13, Kind::Factory, 0, R_EP_CREATE | R_GRANT); // endpoints, which must go back to the pool
        }
        15 => {
            // the chaos monkey: may kill service tasks, can be started only through its endpoint
            install(t, tid, 0, Kind::Admin, 0, R_KILL);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_MONKEY, R_RECV);
            install(t, tid, 2, Kind::MemCtl, 0, R_MEM_STATS | R_MEM_SWAP); // and push random pages out to the swap store
        }
        13 => {
            // system-information screen: draws into the framebuffer, asks the display driver to
            // present, and asks the block driver how big the disk is. Nothing else.
            install(t, tid, 0, Kind::Endpoint, ipc::EP_INFO, R_RECV);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_GPU, R_SEND);
            install(t, tid, 2, Kind::Memory, obj::OBJ_GPU_FB, R_MAP | R_WRITE);
            install(t, tid, 3, Kind::Endpoint, ipc::EP_BLK, R_SEND);
            // and, as the shell: the keyboard driver and one VFS client page
            install(t, tid, 4, Kind::Endpoint, ipc::EP_KBD, R_SEND);
            install(t, tid, 5, Kind::Endpoint, ipc::EP_VFS, R_SEND);
            install(t, tid, 6, Kind::Memory, obj::OBJ_SHM_VFS_SHELL, R_MAP | R_WRITE);
            // the shell starts programs, with file access and a standard-output endpoint it creates itself
            install(t, tid, 8, Kind::Spawn, 0, R_SPAWN | R_SPAWN_FS | R_SPAWN_NET | R_GRANT); // (grant: the desktop hands them on to the programs that start programs)
            install(t, tid, 9, Kind::Factory, 0, R_EP_CREATE | R_GRANT);
            if obj::net_present() {
                // the shell's own network session (`download`, `play URL`): endpoint in slot 12, session page in slot 13
                if let Some(k) = crate::spawn::boot_net_page(tid) {
                    install(t, tid, 12, Kind::Endpoint, ipc::EP_NET, R_SEND);
                    install(t, tid, 13, Kind::Memory, obj::OBJ_SHM_NET_PROG0 + k, R_MAP | R_WRITE);
                }
            }
        }
        19 => {
            // the network service: its endpoint, the packet buffers and ring it shares with the driver (no physical
            // addresses), the right to wake the driver, and the session pages of programs
            install(t, tid, 0, Kind::Endpoint, ipc::EP_NET, R_RECV);
            install(t, tid, 1, Kind::Memory, obj::OBJ_NET_BUFS, R_MAP | R_WRITE);
            install(t, tid, 2, Kind::Memory, obj::OBJ_NET_RING, R_MAP | R_WRITE);
            install(t, tid, 3, Kind::Irq, obj::IRQ_NET, R_SEND);
            for k in 0..8 {
                install(t, tid, 16 + k as usize, Kind::Memory, obj::OBJ_SHM_NET_PROG0 + k, R_MAP | R_WRITE);
            }
        }
        5 => {
            // read-only system filesystem server (EROFS)
            install(t, tid, 0, Kind::Endpoint, ipc::EP_ROFS, R_RECV);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_BLK, R_SEND);
            install(t, tid, 2, Kind::Memory, obj::OBJ_BLK_BUF_ROFS, R_MAP | R_WRITE);
            install(t, tid, 3, Kind::Memory, obj::OBJ_SHM_ROFS, R_MAP | R_WRITE);
            install(t, tid, 4, Kind::Memory, obj::OBJ_SHM_ROFS_VFS, R_MAP | R_WRITE);
        }
        6 => {
            // log-structured data store server
            install(t, tid, 0, Kind::Endpoint, ipc::EP_LFS, R_RECV);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_BLK, R_SEND);
            install(t, tid, 2, Kind::Memory, obj::OBJ_BLK_BUF_LFS, R_MAP | R_WRITE);
            install(t, tid, 3, Kind::Memory, obj::OBJ_SHM_LFS, R_MAP | R_WRITE);
            install(t, tid, 4, Kind::Memory, obj::OBJ_SHM_LFS_VFS, R_MAP | R_WRITE);
        }
        7 => {
            // VFS server: the only task (besides the test conductor) that talks to both filesystems
            install(t, tid, 0, Kind::Endpoint, ipc::EP_VFS, R_RECV);
            install(t, tid, 1, Kind::Endpoint, ipc::EP_ROFS, R_SEND);
            install(t, tid, 2, Kind::Memory, obj::OBJ_SHM_ROFS_VFS, R_MAP | R_WRITE);
            install(t, tid, 3, Kind::Endpoint, ipc::EP_LFS, R_SEND);
            install(t, tid, 4, Kind::Memory, obj::OBJ_SHM_LFS_VFS, R_MAP | R_WRITE);
            install(t, tid, 5, Kind::Memory, obj::OBJ_SHM_VFS_CLIENT, R_MAP | R_WRITE);
            install(t, tid, 6, Kind::Memory, obj::OBJ_SHM_VFS_PROGRAM, R_MAP | R_WRITE);
            install(t, tid, 7, Kind::Memory, obj::OBJ_SHM_VFS_SHELL, R_MAP | R_WRITE);
            // the session pages of programs with file-system access (see spawn.rs)
            for k in 0..8 {
                install(t, tid, 16 + k, Kind::Memory, obj::OBJ_SHM_VFS_PROG0 + k as u8, R_MAP | R_WRITE);
            }
        }
        8 => {
            // an ordinary application: a VFS endpoint and its own page, nothing else
            install(t, tid, 0, Kind::Endpoint, ipc::EP_VFS, R_SEND);
            install(t, tid, 1, Kind::Memory, obj::OBJ_SHM_VFS_PROGRAM, R_MAP | R_WRITE);
            // (and the burners' flag page: in the scheduler tests the otherwise idle program is an Interactive CPU hog)
            install(t, tid, 2, Kind::Memory, obj::OBJ_SHM_SPIN, R_MAP);
        }
        _ => {}
    }
    // what only this machine has (its own devices' windows, the audio service): the platform adds them to the roles that use them
    crate::platform::boot_caps(role, &mut |slot, kind, obj, rights| install(t, tid, slot, kind, obj, rights));
}


/// A random base for a position-independent image (`spawn::FLAG_PIE` and the boot-time programs): a page in `PIE_LO..PIE_HI`
/// with room for `PIE_MAX_IMAGE` after it.
pub fn random_pie_base() -> usize {
    PIE_LO + crate::rand::below((PIE_HI - PIE_LO - PIE_MAX_IMAGE) / PAGE + 1) * PAGE
}

fn build(t: &mut Task, tid: usize) -> Result<(), &'static str> {
    let own = owner(tid);
    // The boot-time programs are one position-independent ELF file, read by the same loader that user programs go through
    // and put at a fresh random base each time a task is built (a restarted service moves).
    let mut plan = metal_loader::plan(USER_IMG).map_err(|_| "bad user image")?;
    if !plan.is_pie() {
        return Err("user image not position-independent");
    }
    let base = random_pie_base();
    plan.rebase(base);
    let l1 = mmu::new_space(own).ok_or("out of memory")?;
    for (va, page, flags) in plan.pages(USER_IMG) {
        let pa = mem::alloc_movable(own, tid, va).ok_or("out of memory")?;
        crate::frame::phys::copy_to(pa, &page[..PAGE]);
        let perm = if flags & metal_loader::X != 0 {
            mmu::Perm::Rx
        } else if flags & metal_loader::W != 0 {
            mmu::Perm::Rw
        } else {
            mmu::Perm::Ro
        };
        mmu::map_user(l1, va, pa, perm, own).map_err(|_| "map failed")?;
        if flags & metal_loader::X != 0 {
            mmu::sync_icache(pa, PAGE);
        }
    }
    let stack_top = random_stack_top();
    for i in 0..STACK_PAGES {
        let pa = mem::alloc_movable(own, tid, stack_top - (i + 1) * PAGE).ok_or("out of memory")?;
        mmu::map_user(l1, stack_top - (i + 1) * PAGE, pa, mmu::Perm::Rw, own).map_err(|_| "map failed")?;
    }
    t.stack_top = stack_top;
    t.heap_base = random_heap_base();
    t.l1 = l1;
    L1[tid].store(l1, Ordering::Release);
    let mut f = frame(tid);
    *f = TrapFrame::ZERO;
    f.x[0] = info(tid).role as u64;
    f.x[1] = tid as u64; // instance number for roles with several copies
    f.sp = stack_top as u64;
    f.elr = plan.entry_addr() as u64;
    f.spsr = 0; // EL0t, interrupts enabled
    t.caps = [Cap::NONE; NCAPS];
    t.maps = [MapRec { used: false, va: 0, pages: 0, node: 0 }; MAX_MAPS];
    t.heap_pages = 0;
    t.tstacks = [(0, 0, 0); crate::spawn::MAX_THREADS];
    t.ring_pa = 0;
    t.pending = [crate::ring::Pending::FREE; crate::ring::MAX_PENDING];
    t.reply_to = None;
    t.callee = None;
    sched::forget_recv_endpoints(tid);
    crate::spawn::restrict_boot_task(tid, info(tid).role);
    boot_caps(t, tid, info(tid).role);
    sched::set_state(tid, State::Ready);
    Ok(())
}

/// Describe a task about to be created at run time in slot `tid` (which is free, so nobody uses its entry).
pub fn set_dynamic_info(tid: usize, role: u8, name: &[u8; 8], class: Class, heap_pages: usize, budget: usize) {
    let n = name.iter().position(|&b| b == 0).unwrap_or(8);
    let name = if n == 0 { Name::new(b"prog") } else { Name::new(&name[..n]) };
    *INFO.write(tid) = Info { role, restartable: false, name, class, pinned: NOT_PINNED, heap_quota: heap_pages, budget };
}

/// Change a program's limits (`QUOTA`): done under its task lock, which is what `heap_grow` reads them under.
pub fn set_limits(tid: usize, heap_quota: usize, budget: usize) {
    let mut i = INFO.write(tid);
    i.heap_quota = heap_quota;
    i.budget = budget;
}

/// Publish the page-table root of a task built at run time (see spawn.rs).
pub fn set_l1(tid: usize, l1: usize) {
    L1[tid].store(l1, Ordering::Release);
}

/// Boot-time only (single core): create a task and make it runnable.
pub fn spawn(role: u8, name: &'static str, class: Class, restartable: bool) -> usize {
    let tid = (0..MAX_TASKS).find(|&i| sched::state(i) == State::Free).expect("no free task slot");
    let core = sched::assign_core(class);
    sched::init_task(tid, core);
    let mut i = Info {
        role,
        restartable,
        name: Name::new(name.as_bytes()),
        class,
        pinned: NOT_PINNED,
        heap_quota: match role {
            0 => CONDUCTOR_HEAP_PAGES,
            // the desktop holds its clients' window surfaces (shared memory it creates, charged to it) as well as its own heap
            13 => DESKTOP_HEAP_PAGES,
            6 | 7 => STORAGE_HEAP_PAGES,
            _ => MAX_HEAP_PAGES,
        },
        budget: if role == 0 { CONDUCTOR_BUDGET_PAGES } else { ROLE_BUDGET_PAGES },
    };
    // Benchmark pair k runs entirely on core k: no cross-core wakeups, no placement noise.
    if role == 10 {
        i.pinned = (tid - FIRST_BENCH_SERVER) as u8;
    } else if role == 11 {
        i.pinned = (tid - FIRST_BENCH_CLIENT) as u8;
    }
    *INFO.write(tid) = i;
    let mut t = lock(tid);
    build(&mut t, tid).expect("failed to build task");
    kprintln!("[kernel] spawned task {tid} '{name}' ({class:?}) on core {core} ({:?})", sched::core_kind(core));
    if crate::rand::deterministic() {
        kprintln!("[kernel] layout of task {tid}: stack top {:#x}, heap {:#x}", t.stack_top, t.heap_base);
    }
    tid
}

/// Every endpoint lock and every task lock, held at once (taken in rank order).
///
/// Operations that must see or change many tasks consistently (a task dying, a capability
/// being revoked everywhere) run under this. They are rare; everything on the hot paths
/// (IPC, memory syscalls) takes only the one or two locks it needs.
pub struct World {
    pub eps: [Guard<'static, Endpoint>; NEP],
    pub tasks: [Guard<'static, Task>; MAX_TASKS],
}

impl World {
    pub fn lock() -> World {
        World { eps: core::array::from_fn(|e| ipc::lock_ep(e as u8)), tasks: core::array::from_fn(lock) }
    }

    /// Kill capabilities derived from `root` everywhere: clear table slots,
    /// unmap shared memory that was mapped through them, free the nodes.
    pub fn revoke_subtree(&mut self, root: crate::cap::NodeId, include_root: bool) -> usize {
        let mut marked = [false; NODES];
        let n = mark_subtree(root, include_root, &mut marked);
        // a task asleep in receive on an endpoint whose receive capability is being revoked must not sleep for ever
        for tid in 0..MAX_TASKS {
            for c in self.tasks[tid].caps {
                if c.kind == Kind::Endpoint && c.rights & R_RECV != 0 && marked[c.node as usize] && self.eps[c.obj as usize].waiter == Some(tid as u8) {
                    self.eps[c.obj as usize].waiter = None;
                    frame(tid).x[0] = crate::syscall::E_BADCAP as u64;
                    sched::wake_if(tid, State::RecvWait(c.obj));
                }
            }
        }
        for t in self.tasks.iter_mut() {
            for c in t.caps.iter_mut() {
                if c.kind != Kind::None && marked[c.node as usize] {
                    *c = Cap::NONE;
                }
            }
            for m in 0..MAX_MAPS {
                let rec = t.maps[m];
                if rec.used && marked[rec.node as usize] {
                    if t.l1 != 0 {
                        for p in 0..rec.pages {
                            mmu::unmap_user(t.l1, rec.va + p * PAGE);
                        }
                    }
                    t.maps[m].used = false;
                }
            }
        }
        crate::ipc::destroy_marked(self, &marked);
        for (i, &m) in marked.iter().enumerate() {
            if m {
                free_node(i as crate::cap::NodeId);
            }
        }
        mmu::flush_tlb();
        self.sweep_orphans();
        n
    }

    /// Shared memory whose creator is gone is freed when nothing holds it any more.
    fn sweep_orphans(&mut self) {
        let mut ids = [0u8; 16];
        for k in 0..obj::shm_orphans(&mut ids) {
            let id = ids[k];
            let held = self.tasks.iter().any(|t| t.caps.iter().any(|c| c.kind == Kind::Memory && c.obj == id && node_alive(c.node)));
            if !held {
                obj::shm_destroy(id);
            }
        }
    }

    /// Tear a task down: kill its capability subtrees and every link to it. (Its memory is `reclaim_space`, once
    /// nothing runs in the address space any more.)
    fn reset(&mut self, tid: usize) {
        for slot in 0..NCAPS {
            let c = self.tasks[tid].caps[slot];
            if c.kind != Kind::None && node_alive(c.node) {
                // memory from MC_ALLOC is private to its task (the capability has no GRANT right): its frames go
                // below with the rest, but the object-table entry would stay taken for ever (found by the fuzzer)
                if c.kind == Kind::Memory && obj::is_dynamic(c.obj) {
                    match obj::get(c.obj) {
                        // The creator of shared memory is going. With no holders the memory goes too; with holders it
                        // lives on without a creator until the last of them lets go (a holder's own copy, or a thread's,
                        // just goes with the revocation below).
                        Some(o) if o.kind == obj::ObjKind::Shared => {
                            if o.root == c.node {
                                let mut holders = [false; NODES];
                                if mark_subtree(c.node, false, &mut holders) > 0 {
                                    obj::shm_orphan(c.obj);
                                    crate::cap::detach_children(c.node);
                                    self.revoke_subtree(c.node, true);
                                } else {
                                    self.revoke_subtree(c.node, true);
                                    obj::shm_destroy(c.obj);
                                }
                            }
                        }
                        _ => obj::release(c.obj),
                    }
                }
                if node_alive(c.node) {
                    self.revoke_subtree(c.node, true);
                }
            }
        }
        let t = &mut self.tasks[tid];
        t.caps = [Cap::NONE; NCAPS];
        t.reply_to = None;
        t.callee = None;
        crate::ring::reset_task(t);
        for o in 0..MAX_TASKS {
            if self.tasks[o].reply_to.map_or(false, |(c, _)| c == tid as u8) {
                self.tasks[o].reply_to = None;
            }
        }
        for (i, e) in self.eps.iter_mut().enumerate() {
            if e.waiter == Some(tid as u8) {
                e.waiter = None;
            }
            e.purge_calls_from(i as u8, tid as u8);
        }
        crate::irq::forget(tid as u8);
        crate::cap::forget_granter(tid);
        crate::cap::forget_revoked(tid);
    }

    /// Free an address space: its mappings, the pages in the swap store and every frame it owns. The last task that
    /// ran in it calls this.
    fn reclaim_space(&mut self, leader: usize) -> usize {
        let t = &mut self.tasks[leader];
        t.maps = [MapRec { used: false, va: 0, pages: 0, node: 0 }; MAX_MAPS];
        crate::swap::release_task(t.l1); // pages in the swap store go with the frames
        let n = mem::free_owned(owner(leader));
        crate::spawn::forget_usage(leader);
        mmu::flush_tlb();
        n
    }
}

/// Condemn every live task running `role` (see `sched::doom`).
pub fn condemn_role(role: u8, why: sched::Doom) -> usize {
    let mut n = 0;
    for tid in 0..MAX_TASKS {
        // a dead task has nothing to kill (and a stale flag on it would hit its next instance)
        if !matches!(sched::state(tid), State::Free | State::Dead) && info(tid).role == role {
            sched::doom(tid, why);
            n += 1;
        }
    }
    n
}

/// A task faulted or exited: reclaim its resources, unblock anyone waiting
/// on it, and restart it if policy says so. A thread that dies this way takes the whole program with it.
pub fn die(tid: usize, why: &str) {
    die_as(tid, why, true, None)
}

/// A thread asked to end (THREAD_EXIT): only it goes. Once its slot is free, `exit_word` (address, value) is stored in
/// its program's memory and everyone waiting on it woken: a thread that has been joined is entirely gone.
pub fn die_thread(tid: usize, exit_word: (usize, u32)) {
    die_as(tid, "thread exit", false, Some(exit_word))
}

fn die_as(tid: usize, why: &str, program_dies: bool, exit_word: Option<(usize, u32)>) {
    // This core is still running on the dying task's page tables: leave them before they are freed.
    mmu::use_kernel_tables();
    let leader = aspace(tid);
    if !(crate::spawn::is_program(tid) && why == "exited") && !(leader != tid && !program_dies) {
        kprintln!("[kernel] task {tid} '{}' terminated: {why}", info(tid).name);
    }
    let mut w = World::lock();
    for c in 0..MAX_TASKS {
        if w.tasks[c].callee == Some(tid as u8) && sched::state(c) == State::CallWait {
            w.tasks[c].callee = None;
            frame(c).x[0] = crate::syscall::E_DEAD as u64;
            sched::wake(c);
        }
    }
    crate::ring::fail_calls_to(&mut w, tid);
    for sid in obj::sids_of_role(info(tid).role).into_iter().flatten() {
        crate::smmu::detach(sid); // the device is cut off until a new driver instance is given its pages
    }
    w.reset(tid);
    // Who else still runs in this address space? If the program is dying they are condemned, and the memory stays
    // until the last of them is gone (a thread on another core may be using it right now).
    let mut others = 0;
    for m in members(leader).filter(|&m| m != tid) {
        others += 1;
        if program_dies {
            sched::doom(m, sched::Doom::Parent);
        }
    }
    let last = others == 0;
    let freed = if last { w.reclaim_space(leader) } else { 0 };
    sched::clear_doom(tid);
    L1[tid].store(0, Ordering::Release);
    sched::clear_inherit(tid);
    sched::forget_task(tid);
    if leader != tid {
        // a thread leaves nothing behind for anyone to collect: its slot is free at once
        crate::spawn::thread_gone(tid, leader);
        if !last {
            crate::spawn::free_thread_stack(&mut w.tasks[leader], tid);
        }
        if let (Some((va, value)), false) = (exit_word, last) {
            crate::thread::store_and_wake(&w.tasks[leader], leader, va, value);
        }
    } else {
        sched::set_state(tid, State::Dead);
    }
    let now = crate::timer::ticks();
    if now.saturating_sub(LAST_RESTART[tid].load(Ordering::Relaxed)) >= STABLE_TICKS {
        STREAK[tid].store(0, Ordering::Relaxed);
    }
    // any task's children go with it; a program's own slot is freed or kept for its parent to collect once the
    // last of its threads is gone
    crate::spawn::on_death(&mut w, tid, if last { Some(leader) } else { None });
    if crate::spawn::is_program(tid) || leader != tid {
        // (no restarts: a program is its parent's business)
    } else if info(tid).restartable && STREAK[tid].load(Ordering::Relaxed) < MAX_RESTARTS {
        match build(&mut w.tasks[tid], tid) {
            Ok(()) => {
                STREAK[tid].fetch_add(1, Ordering::Relaxed);
                LAST_RESTART[tid].store(now, Ordering::Relaxed);
                let n = RESTARTS[tid].fetch_add(1, Ordering::Relaxed) + 1;
                kprintln!("[kernel] reclaimed {freed} pages; restarted '{}' (restart #{n})", info(tid).name);
            }
            Err(e) => kprintln!("[kernel] restart of '{}' failed: {e}", info(tid).name),
        }
    } else {
        kprintln!("[kernel] reclaimed {freed} pages from '{}'", info(tid).name);
    }
}
