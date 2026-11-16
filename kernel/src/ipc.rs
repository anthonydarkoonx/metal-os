//! Kernel endpoints: bounded message queues with at most one blocked receiver,
//! each behind its own lock (rank `R_EP + endpoint`).
//!
//! Ordering rules that make the lock-free-looking hand-offs safe:
//!  * a receiver registers as waiter and goes to sleep inside one critical
//!    section of the endpoint lock; a sender completes its delivery inside the
//!    same lock. So a wakeup can neither be lost nor arrive early.
//!  * a synchronous caller is marked blocked *before* its message becomes
//!    visible to the receiver (delivered or queued): the receiver may run on
//!    another core at once and reply, and the reply must find the caller blocked.
//!  * the sender's priority is recorded for priority inheritance at the moment
//!    of delivery/queueing, in atomics, so the scheduler never has to look here.
#![forbid(unsafe_code)]
use crate::sched::{self, NO_PRIO};
use crate::sync::{Guard, SpinLock, R_EP};
use crate::task::{self, frame, State};
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicU8, Ordering};
use crate::cap::NodeId;

/// Endpoints 0..21 are the boot-time services'; 21..37 are created at run time (`EP_CREATE`, see below).
pub const NEP: usize = 38;
pub const EP_FIRST_DYNAMIC: u8 = 22;
pub const EP_SERVICE: u8 = 0;
pub const EP_DRIVER: u8 = 1;
pub const EP_BLK: u8 = 2;
pub const EP_ROFS: u8 = 3;
pub const EP_LFS: u8 = 4;
pub const EP_VFS: u8 = 5;
/// Benchmark pairs: server request endpoint and client start endpoint, per pair 0..4.
pub const EP_BENCH_SRV: u8 = 6;
pub const EP_BENCH_GO: u8 = 10;
/// The display driver.
pub const EP_GPU: u8 = 16;
/// The system-information screen: told to start once the display tests are done.
pub const EP_INFO: u8 = 17;
/// The keyboard driver, polled by the shell.
pub const EP_KBD: u8 = 18;
/// Chaos test: the conductor tells the monkey when to start.
pub const EP_MONKEY: u8 = 19;
/// The network service (`netd`): programs call it, the network driver tells it about frames and ticks.
pub const EP_NET: u8 = 20;
/// The audio service (a platform with an audio device runs it as role 20; others have none).
pub const EP_AUDIO: u8 = 21;
pub const BENCH_PAIRS: usize = 4;
const QLEN: usize = 8;

#[derive(Clone, Copy)]
pub struct Msg {
    pub sender: u8,
    pub call: bool,
    /// 0 = synchronous caller blocked in IPC_CALL; otherwise 1 + index of the async call in the sender's ring.
    pub tag: u8,
    pub w: [u64; 3],
}

const NO_MSG: Msg = Msg { sender: 0, call: false, tag: 0, w: [0; 3] };

pub struct Endpoint {
    q: [Msg; QLEN],
    head: usize,
    len: usize,
    /// Receiver sleeping on this endpoint.
    pub waiter: Option<u8>,
}

impl Endpoint {
    /// Drop calls still queued from `tid`, a task that is dying. Nobody waits for their replies any
    /// more, and the task id is about to be reused by a fresh instance, which must not be confused
    /// with them (a server would link the stale call to the new instance and reply into its state).
    /// Plain notifications (`call == false`) stay: they were sent and are still deliverable.
    pub fn purge_calls_from(&mut self, e: u8, tid: u8) {
        let mut kept = [NO_MSG; QLEN];
        let mut n = 0;
        for k in 0..self.len {
            let m = self.q[(self.head + k) % QLEN];
            if !(m.call && m.sender == tid) {
                kept[n] = m;
                n += 1;
            }
        }
        self.q = kept;
        self.head = 0;
        self.len = n;
        self.refresh_best(e);
    }
}

/// Owner capability node of each run-time endpoint (`NO_OWNER`: free). The endpoint lives as long as that node.
const NO_OWNER: u8 = u8::MAX;
const NO_OWNER_NODE: NodeId = NodeId::MAX;
static EP_NODE: [AtomicU16; NEP] = [const { AtomicU16::new(NO_OWNER_NODE) }; NEP];
/// ... and the task that created it (for its quota).
static EP_TASK: [AtomicU8; NEP] = [const { AtomicU8::new(NO_OWNER) }; NEP];
/// Most run-time endpoints one task may own at a time.
pub const MAX_OWNED_EPS: usize = 4;

/// The capability node that is the root of endpoint `e` (the creator's own capability), if it is in use.
pub fn root_node(e: u8) -> Option<NodeId> {
    let n = EP_NODE.get(e as usize)?.load(Ordering::SeqCst);
    (n != NO_OWNER_NODE).then_some(n)
}

/// The task that created run-time endpoint `e` (`None` for a boot endpoint or a free one).
pub fn dynamic_owner(e: u8) -> Option<u8> {
    if e < EP_FIRST_DYNAMIC || e as usize >= NEP {
        return None;
    }
    let t = EP_TASK[e as usize].load(Ordering::SeqCst);
    (EP_NODE[e as usize].load(Ordering::SeqCst) != NO_OWNER_NODE && t != NO_OWNER).then_some(t)
}

/// Claim a free run-time endpoint for `task`, owned by capability node `node`. None if the task is at its quota or
/// none is free.
pub fn claim_dynamic(task: usize, node: NodeId) -> Option<u8> {
    let owned = (EP_FIRST_DYNAMIC as usize..NEP).filter(|&e| EP_NODE[e].load(Ordering::SeqCst) != NO_OWNER_NODE && EP_TASK[e].load(Ordering::SeqCst) == task as u8).count();
    if owned >= MAX_OWNED_EPS {
        return None;
    }
    for e in EP_FIRST_DYNAMIC as usize..NEP {
        if EP_NODE[e].compare_exchange(NO_OWNER_NODE, node, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            EP_TASK[e].store(task as u8, Ordering::SeqCst);
            return Some(e as u8);
        }
    }
    None
}

/// The owner capabilities in `marked` are being revoked (every endpoint and task is locked): the run-time endpoints
/// they own cease to exist. Whoever waits on them or in their queues is told, so nobody sleeps for ever.
pub fn destroy_marked(w: &mut task::World, marked: &[bool; crate::cap::NODES]) {
    for e in EP_FIRST_DYNAMIC as usize..NEP {
        let n = EP_NODE[e].load(Ordering::SeqCst);
        if n == NO_OWNER_NODE || !marked[n as usize] {
            continue;
        }
        let dead = crate::syscall::E_DEAD;
        let ep = &mut w.eps[e];
        if let Some(r) = ep.waiter.take() {
            frame(r as usize).x[0] = dead as u64;
            sched::wake_if(r as usize, State::RecvWait(e as u8));
        }
        let (head, len) = (ep.head, ep.len);
        let queued: [Msg; QLEN] = core::array::from_fn(|k| ep.q[(head + k) % QLEN]);
        ep.head = 0;
        ep.len = 0;
        ep.refresh_best(e as u8);
        for m in queued.iter().take(len) {
            let s = m.sender as usize;
            if !m.call {
                continue; // a plain notification nobody will read
            }
            if m.tag == 0 {
                frame(s).x[0] = dead as u64;
                sched::wake_if(s, State::CallWait);
            } else {
                crate::ring::fail_queued(&mut w.tasks[s], s, m.tag, dead);
            }
        }
        EP_TASK[e].store(NO_OWNER, Ordering::SeqCst);
        EP_NODE[e].store(NO_OWNER_NODE, Ordering::SeqCst);
    }
}

static EPS: [SpinLock<Endpoint>; NEP] =
    [const { SpinLock::new(Endpoint { q: [NO_MSG; QLEN], head: 0, len: 0, waiter: None }) }; NEP];

/// Best (lowest) priority among the senders of requests queued on each endpoint.
static BEST: [AtomicU8; NEP] = [const { AtomicU8::new(NO_PRIO) }; NEP];
/// Earliest deadline among those senders that are real-time (or serving a real-time task).
static BEST_DEADLINE: [AtomicU64; NEP] = [const { AtomicU64::new(u64::MAX) }; NEP];

pub fn lock_ep(e: u8) -> Guard<'static, Endpoint> {
    EPS[e as usize].lock(R_EP + e as u16)
}

pub fn best_queued(e: u8) -> u8 {
    BEST[e as usize].load(Ordering::Relaxed)
}

pub fn best_queued_deadline(e: u8) -> u64 {
    BEST_DEADLINE[e as usize].load(Ordering::Relaxed)
}

impl Endpoint {
    fn refresh_best(&self, e: u8) {
        let mut best = NO_PRIO;
        let mut deadline = u64::MAX;
        for k in 0..self.len {
            let sender = self.q[(self.head + k) % QLEN].sender as usize;
            best = best.min(sched::priority_to_pass_on(sender));
            deadline = deadline.min(sched::deadline_to_pass_on(sender));
        }
        BEST[e as usize].store(best, Ordering::Relaxed);
        BEST_DEADLINE[e as usize].store(deadline, Ordering::Relaxed);
    }
}

/// Record who `rcv` now serves: reply link, the caller's "being served by" link, inherited priority.
/// The caller is blocked already (see module docs); its task lock is taken here together with ours.
fn link_call(rcv: usize, m: &Msg) {
    let sender = m.sender as usize;
    if rcv == sender {
        let mut t = task::lock(rcv);
        t.reply_to = Some((m.sender, m.tag));
        t.callee = Some(rcv as u8);
    } else {
        // both tasks, in ascending order
        let (lo, hi) = if rcv < sender { (rcv, sender) } else { (sender, rcv) };
        let mut glo = task::lock(lo);
        let mut ghi = task::lock(hi);
        let (r, s) = if rcv < sender { (&mut *glo, &mut *ghi) } else { (&mut *ghi, &mut *glo) };
        r.reply_to = Some((m.sender, m.tag));
        set_callee(s, m.tag, rcv);
    }
    sched::inherit_from(rcv, sender);
}

fn set_callee(sender: &mut task::Task, tag: u8, rcv: usize) {
    if tag == 0 {
        sender.callee = Some(rcv as u8);
    } else {
        crate::ring::set_callee(sender, tag, rcv);
    }
}

/// Write a message into a receiver's saved registers (its pending IPC_RECV result).
fn deliver_regs(rcv: usize, m: &Msg) {
    let f = frame(rcv);
    f.x[0] = 0;
    f.x[1] = m.sender as u64;
    f.x[2] = m.w[0];
    f.x[3] = m.w[1];
    f.x[4] = m.w[2];
    f.x[5] = m.call as u64;
}

pub enum SendErr {
    Full,
}

/// Send `m` (from task `cur`) to endpoint `e`. For a synchronous call the sender is left
/// blocked in `CallWait`; the caller of this function then reports `Block`.
pub fn send(cur: usize, e: u8, m: Msg) -> Result<(), SendErr> {
    let mut ep = lock_ep(e);
    let sync_call = m.call && m.tag == 0;
    if let Some(r) = ep.waiter.take() {
        let r = r as usize;
        deliver_regs(r, &m);
        if m.call {
            link_call(r, &m);
        }
        // sender blocked first, receiver runnable second, in one scheduler critical section
        sched::block_and_wake(sync_call.then_some((cur, State::CallWait)), Some(r));
        return Ok(());
    }
    if ep.len == QLEN {
        return Err(SendErr::Full);
    }
    let at = (ep.head + ep.len) % QLEN;
    ep.q[at] = m;
    ep.len += 1;
    ep.refresh_best(e);
    if sync_call {
        sched::set_state(cur, State::CallWait);
    }
    Ok(())
}

pub enum Recv {
    Got,
    Blocked,
    Busy,
    /// Nothing queued and the caller asked not to wait.
    Empty,
}

/// Receive on endpoint `e` for task `me`. On `Got` the message is already in `me`'s registers;
/// on `Blocked` the task is registered and asleep in `RecvWait`.
pub fn recv(me: usize, e: u8, block: bool) -> Recv {
    let mut ep = lock_ep(e);
    if ep.len > 0 {
        let m = ep.q[ep.head];
        ep.head = (ep.head + 1) % QLEN;
        ep.len -= 1;
        ep.refresh_best(e);
        deliver_regs(me, &m);
        if m.call {
            link_call(me, &m);
        }
        return Recv::Got;
    }
    if !block {
        return Recv::Empty;
    }
    if ep.waiter.is_some() {
        return Recv::Busy;
    }
    ep.waiter = Some(me as u8);
    sched::set_state(me, State::RecvWait(e));
    Recv::Blocked
}

/// Reply from `server` to the caller it is currently serving.
pub fn reply(server: usize, w: [u64; 3]) -> bool {
    let link = task::lock(server).reply_to.take();
    let Some((c, tag)) = link else { return false };
    sched::clear_inherit(server);
    let c = c as usize;
    let mut tc = task::lock(c);
    if tag != 0 {
        return crate::ring::complete_call(&mut tc, c, tag, server, w);
    }
    if tc.callee != Some(server as u8) || sched::state(c) != State::CallWait {
        return false; // caller died or moved on
    }
    tc.callee = None;
    let f = frame(c);
    f.x[0] = 0;
    f.x[1] = w[0];
    f.x[2] = w[1];
    f.x[3] = w[2];
    sched::wake_if(c, State::CallWait) // publishes the registers above
}
