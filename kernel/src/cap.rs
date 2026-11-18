//! Capabilities. Every capability sits in a derivation tree (`node`), so
//! revoking a capability atomically kills everything derived from it.
#![forbid(unsafe_code)]
use crate::sync::{SpinLock, R_NODES};
use core::sync::atomic::{AtomicBool, Ordering};

pub const NCAPS: usize = 32;
/// A capability node's number; `NO_PARENT` (the largest value) means "no parent". (Widened from `u8` on the ROCK 3C, whose boot-time services and drivers
/// already use two thirds of 254 nodes, leaving too little for two windowed programs at once.)
pub type NodeId = u16;
pub const NODES: usize = 1024;

pub type Rights = u16;

pub const R_SEND: Rights = 1;
pub const R_RECV: Rights = 2;
pub const R_GRANT: Rights = 4;
pub const R_MAP: Rights = 8;
pub const R_WRITE: Rights = 16;
/// May learn the physical address of DMA / MMIO objects (driver-only).
pub const R_INFO: Rights = 32;
/// Memory-control capability rights: read allocator statistics / run compaction / allocate contiguous memory.
pub const R_MEM_STATS: Rights = 1 << 6;
pub const R_MEM_COMPACT: Rights = 1 << 7;
pub const R_MEM_ALLOC: Rights = 1 << 8;
/// May evict pages to the compressed swap store (the policy of when to do so lives in userspace).
pub const R_MEM_SWAP: Rights = 1 << 10;
/// On a MemCtl capability: may act as the pager (see `pager.rs`): take the disk-writeback and page-in jobs.
pub const R_MEM_PAGER: Rights = 1 << 5;
/// Administrative right: may kill (and so restart) the system's restartable service tasks. Held only by the
/// chaos-test monkey.
pub const R_KILL: Rights = 1 << 9;
/// Administrative right: may revoke a capability held by a program that was started at run time (`REVOKE_OF`), never one that is the root
/// of its object. Held by the task manager only, through the program manager.
pub const R_REVOKE: Rights = 1 << 10; // (rights mean what the kind says: this one is for `Admin`, as `R_KILL` is)
/// May give itself a real-time reservation (see `sched.rs`): a guaranteed share of a core.
pub const R_RT: Rights = 1 << 11;
/// Switch the machine off or restart it (`POWER`). A right of the `Admin` kind (like `R_REVOKE` and `R_KILL`), sharing a bit with `R_RT` which belongs to `Sched`.
pub const R_POWER: Rights = 1 << 11;
/// May create programs (SPAWN). Delegated with the grant right so that programs can start programs.
pub const R_SPAWN: Rights = 1 << 12;
/// On a Spawn capability: programs it starts may be given file-system access.
pub const R_SPAWN_FS: Rights = 1 << 13;
/// On a Factory capability: may create endpoints (EP_CREATE).
pub const R_EP_CREATE: Rights = 1 << 14;
/// On a Spawn capability: programs it starts may be given network access.
pub const R_SPAWN_NET: Rights = 1 << 15;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    None,
    Endpoint,
    Memory,
    Console,
    Irq,
    /// Physical-memory management (statistics, compaction, contiguous allocation).
    MemCtl,
    /// Administrative authority over service tasks (see `R_KILL`).
    Admin,
    /// Scheduling authority (see `R_RT`).
    Sched,
    /// Authority to create programs (see spawn.rs, `R_SPAWN`).
    Spawn,
    /// Authority to create kernel objects at run time (endpoints: `R_EP_CREATE`).
    Factory,
}

#[derive(Clone, Copy)]
pub struct Cap {
    pub kind: Kind,
    /// Index of the kernel object (endpoint id / memory object id).
    pub obj: u8,
    pub rights: Rights,
    pub node: NodeId,
}

impl Cap {
    pub const NONE: Cap = Cap { kind: Kind::None, obj: 0, rights: 0, node: 0 };
}

pub const NO_PARENT: NodeId = NodeId::MAX;
/// (task ids are `u8`: this is "granted by no task")
const NO_GRANTER: u8 = u8::MAX;

#[derive(Clone, Copy)]
struct Node {
    used: bool,
    parent: NodeId,
    /// The task that created this node with `CAP_GRANT` (`NO_PARENT` if it was not made by a grant, or the granter is gone).
    granter: u8,
    /// The program (address space) charged for this node when it was made for one (`mint_for`), else `NO_GRANTER`.
    owner: u8,
}

/// Most nodes one task may have outstanding through `CAP_GRANT`, and the nodes `CAP_GRANT` may never take: the rest are for
/// restarting a service and starting a program, which must not fail because a task with the grant right filled the table.
pub const GRANT_QUOTA: usize = 40;
pub const GRANT_RESERVE: usize = 48;

/// Mirror of `used`, readable without the lock: every IPC checks its capability is alive.
static ALIVE: [AtomicBool; NODES] = [const { AtomicBool::new(false) }; NODES];

static NODE_TABLE: SpinLock<[Node; NODES]> = SpinLock::new([Node { used: false, parent: NO_PARENT, granter: NO_GRANTER, owner: NO_GRANTER }; NODES]);

pub fn mint_node(parent: Option<NodeId>) -> Option<NodeId> {
    let mut t = NODE_TABLE.lock(R_NODES);
    let i = t.iter().position(|n| !n.used)?;
    t[i] = Node { used: true, parent: parent.unwrap_or(NO_PARENT), granter: NO_GRANTER, owner: NO_GRANTER };
    ALIVE[i].store(true, Ordering::Release);
    Some(i as NodeId)
}

/// Most nodes one program (all its threads together) may hold that it made for itself: its own memory and endpoints, its threads'
/// copies of capabilities, what it gives the programs it starts. Programs may also never take the last `GRANT_RESERVE` nodes:
/// those are for restarting a boot service (which cannot fail) and for starting programs.
pub const PROGRAM_NODE_QUOTA: usize = 192;

/// A node for `owner`, a program's address space (charged to it, and refused over `PROGRAM_NODE_QUOTA` or into the reserve), or
/// for anything else (a boot-time task: unlimited, as `mint_node`).
pub fn mint_for(owner: usize, parent: Option<NodeId>) -> Option<NodeId> {
    let charged = crate::spawn::is_program(owner);
    let o = owner as u8;
    let mut t = NODE_TABLE.lock(R_NODES);
    if charged && (t.iter().filter(|n| n.used && n.owner == o).count() >= PROGRAM_NODE_QUOTA || t.iter().filter(|n| !n.used).count() <= GRANT_RESERVE) {
        return None;
    }
    let i = t.iter().position(|n| !n.used)?;
    t[i] = Node { used: true, parent: parent.unwrap_or(NO_PARENT), granter: NO_GRANTER, owner: if charged { o } else { NO_GRANTER } };
    ALIVE[i].store(true, Ordering::Release);
    Some(i as NodeId)
}

/// A node made by `CAP_GRANT` from task `granter`: refused when the task already has `GRANT_QUOTA` outstanding, or when
/// fewer than `GRANT_RESERVE` nodes would be left for everyone else.
pub fn mint_granted(parent: NodeId, granter: usize) -> Option<NodeId> {
    let mut t = NODE_TABLE.lock(R_NODES);
    let g = granter as u8;
    if t.iter().filter(|n| n.used && n.granter == g).count() >= GRANT_QUOTA || t.iter().filter(|n| !n.used).count() <= GRANT_RESERVE {
        return None;
    }
    let i = t.iter().position(|n| !n.used)?;
    t[i] = Node { used: true, parent, granter: g, owner: NO_GRANTER };
    ALIVE[i].store(true, Ordering::Release);
    Some(i as NodeId)
}

/// The node `n` hangs under, if it is in use and has one.
pub fn parent_of(n: NodeId) -> Option<NodeId> {
    let t = NODE_TABLE.lock(R_NODES);
    let node = t.get(n as usize)?;
    (node.used && node.parent != NO_PARENT).then_some(node.parent)
}

/// A capability that was revoked from a program by `REVOKE_OF`, kept so that the same one can be given back (`RESTORE_OF`): only what is in
/// this log can be restored, so restoring cannot make up authority. The parent is the node it hung under; it is given back as a new child of it.
#[derive(Clone, Copy)]
pub struct Revoked {
    pub id: u32,
    pub task: u8,
    pub slot: u8,
    pub kind: Kind,
    pub obj: u8,
    pub rights: Rights,
    /// The node it hung under, or `NO_PARENT` for a capability the kernel made itself (a file or network session of a program).
    pub parent: NodeId,
    /// Where the program had it mapped (0 pages: not mapped), so that giving it back can map it again.
    pub map_va: usize,
    pub map_pages: usize,
}

const REVOKED_LOG: usize = 16;

struct Log {
    entries: [Option<Revoked>; REVOKED_LOG],
    next: usize,
    next_id: u32,
}

static REVOKED: SpinLock<Log> = SpinLock::new(Log { entries: [None; REVOKED_LOG], next: 0, next_id: 1 });

/// Remember a revoked capability of `task` (the oldest entry goes when the log is full); returns its id.
pub fn log_revoked(task: usize, slot: usize, c: Cap, parent: NodeId, map: (usize, usize)) -> u32 {
    let mut l = REVOKED.lock(R_NODES);
    let id = l.next_id;
    l.next_id = l.next_id.wrapping_add(1).max(1);
    let at = l.next;
    l.entries[at] = Some(Revoked { id, task: task as u8, slot: slot as u8, kind: c.kind, obj: c.obj, rights: c.rights, parent, map_va: map.0, map_pages: map.1 });
    l.next = (at + 1) % REVOKED_LOG;
    id
}

/// The `n`th revoked capability of `task`, oldest first, and how many there are.
pub fn revoked_nth(task: usize, n: usize) -> (Option<Revoked>, usize) {
    let l = REVOKED.lock(R_NODES);
    let mut mine: [Option<Revoked>; REVOKED_LOG] = [None; REVOKED_LOG];
    let mut count = 0;
    for k in 0..REVOKED_LOG {
        let e = l.entries[(l.next + k) % REVOKED_LOG];
        if let Some(r) = e.filter(|r| r.task as usize == task) {
            mine[count] = Some(r);
            count += 1;
        }
    }
    (mine.get(n).copied().flatten(), count)
}

/// The log entry with this id, if it is for `task`.
pub fn revoked_by_id(task: usize, id: u32) -> Option<Revoked> {
    REVOKED.lock(R_NODES).entries.iter().flatten().find(|r| r.id == id && r.task as usize == task).copied()
}

/// Drop an entry (it was given back).
pub fn forget_revoked_id(id: u32) {
    let mut l = REVOKED.lock(R_NODES);
    for e in l.entries.iter_mut() {
        if e.is_some_and(|r| r.id == id) {
            *e = None;
        }
    }
}

/// A task is gone: what was revoked from it can no longer be given back.
pub fn forget_revoked(tid: usize) {
    let mut l = REVOKED.lock(R_NODES);
    for e in l.entries.iter_mut() {
        if e.is_some_and(|r| r.task as usize == tid) {
            *e = None;
        }
    }
}

/// A node for a capability given back by `RESTORE_OF`: a child of `parent`, not counted against anyone's grant quota (the restorer is the trusted
/// administrator, and each give-back replaces a node a revoke freed), but never taking the nodes kept in reserve for restarting a service.
pub fn mint_restored(parent: NodeId) -> Option<NodeId> {
    let mut t = NODE_TABLE.lock(R_NODES);
    if t.iter().filter(|n| !n.used).count() <= GRANT_RESERVE {
        return None;
    }
    let i = t.iter().position(|n| !n.used)?;
    t[i] = Node { used: true, parent, granter: NO_GRANTER, owner: NO_GRANTER };
    ALIVE[i].store(true, Ordering::Release);
    Some(i as NodeId)
}

/// A task is gone: what it granted stays with the receivers, but no longer counts against the task id's next owner.
pub fn forget_granter(tid: usize) {
    let mut t = NODE_TABLE.lock(R_NODES);
    for n in t.iter_mut() {
        if n.used && n.granter == tid as u8 {
            n.granter = NO_GRANTER;
        }
    }
}

/// The children of `root` no longer depend on it: they become roots of their own trees (so revoking `root` leaves them).
pub fn detach_children(root: NodeId) {
    let mut t = NODE_TABLE.lock(R_NODES);
    for n in t.iter_mut() {
        if n.used && n.parent == root {
            n.parent = NO_PARENT;
        }
    }
}

pub fn node_alive(n: NodeId) -> bool {
    ALIVE[n as usize].load(Ordering::Acquire)
}

pub fn free_node(n: NodeId) {
    let mut t = NODE_TABLE.lock(R_NODES);
    t[n as usize].used = false;
    ALIVE[n as usize].store(false, Ordering::Release);
}

/// Mark `root`'s descendants (and `root` itself when `include_root`) in `out`.
/// Returns how many nodes were marked.
pub fn mark_subtree(root: NodeId, include_root: bool, out: &mut [bool; NODES]) -> usize {
    let t = NODE_TABLE.lock(R_NODES);
    *out = [false; NODES];
    out[root as usize] = true;
    loop {
        let mut changed = false;
        for i in 0..NODES {
            let p = t[i].parent;
            if t[i].used && !out[i] && p != NO_PARENT && out[p as usize] {
                out[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !include_root {
        out[root as usize] = false;
    }
    out.iter().filter(|&&m| m).count()
}

/// Capability nodes in use (a margin check: `install` panics if they run out).
pub fn nodes_in_use() -> usize {
    ALIVE.iter().filter(|a| a.load(Ordering::Relaxed)).count()
}
