//! Interrupt forwarding: a task holding an IRQ capability can block until its
//! device interrupts. The line is masked when it fires and re-enabled by the
//! next `IRQ_WAIT`, so a slow driver can never be flooded.
#![forbid(unsafe_code)]
use crate::sched;
use crate::sync::{SpinLock, R_IRQ};
use crate::task::{frame, State};
use crate::{gic, timer};

/// Device interrupt slots: the block, display and network devices, and one for the platform's own (audio).
pub const NIRQ: usize = 4;

#[derive(Clone, Copy)]
struct Irq {
    used: bool,
    gic_id: u32,
    waiter: Option<u16>,
    pending: bool,
    /// Raised by software (`raise`) while nobody waited.
    soft: bool,
}

static IRQS: SpinLock<[Irq; NIRQ]> =
    SpinLock::new([Irq { used: false, gic_id: 0, waiter: None, pending: false, soft: false }; NIRQ]);

pub fn register(idx: usize, gic_id: u32) {
    gic::configure_spi(gic_id);
    IRQS.lock(R_IRQ)[idx] = Irq { used: true, gic_id, waiter: None, pending: false, soft: false };
}

/// Top-level IRQ handler (EL0 preemption path and idle path). Returns true for a jiffy tick (a timer interrupt that is only a slice end or a real-time event is not one).
pub fn handle() -> bool {
    let iar = gic::ack();
    let id = iar & 0x3FF;
    if id >= 1020 {
        return false; // spurious
    }
    let mut tick = false;
    if id == timer::TIMER_PPI {
        let core = crate::smp::cpu_id();
        sched::count_timer_irq();
        tick = timer::on_irq(core);
        if tick {
            sched::tick();
        }
        if tick && core == 0 {
            crate::ring::expire_timeouts(); // check async timeouts once per jiffy
        }
    } else if id == crate::smmu::EVENT_IRQ {
        crate::smmu::event_irq();
    } else if crate::platform::irq(id) {
        // (a device interrupt the platform handled itself)
    } else if id >= 32 {
        fire(id);
    } // ids 0..15 are IPIs: their only job is to wake an idle core
    crate::rand::note_event();
    gic::eoi(iar);
    tick
}

fn fire(id: u32) {
    gic::disable(id);
    let mut irqs = IRQS.lock(R_IRQ);
    for irq in irqs.iter_mut().filter(|i| i.used && i.gic_id == id) {
        match irq.waiter.take() {
            Some(w) => {
                frame(w as usize).x[0] = 0; // the blocked IRQ_WAIT returns 0
                sched::wake(w as usize);
            }
            None => irq.pending = true,
        }
    }
}

pub enum Wait {
    Ready,
    Blocked,
    Busy,
}

pub fn wait(idx: u16, me: usize, timeout: u64) -> Wait {
    let mut irqs = IRQS.lock(R_IRQ);
    let irq = &mut irqs[idx as usize];
    if irq.waiter.is_some() {
        return Wait::Busy;
    }
    gic::enable(irq.gic_id);
    if irq.pending || irq.soft {
        irq.pending = false;
        irq.soft = false;
        return Wait::Ready;
    }
    irq.waiter = Some(me as u16);
    sched::set_irq_deadline(me, if timeout == 0 { 0 } else { timer::counter().saturating_add(timeout.saturating_mul(timer::interval())) });
    sched::set_state(me, State::IrqWait(idx as u8));
    Wait::Blocked
}

/// Raise the interrupt line from software (`IRQ_RAISE`, needs the send right on the line's capability): the task blocked
/// in `IRQ_WAIT` on it wakes as if the device had interrupted; if nobody waits, the next wait returns at once.
pub fn raise(idx: u16) {
    let mut irqs = IRQS.lock(R_IRQ);
    let irq = &mut irqs[idx as usize];
    match irq.waiter.take() {
        Some(w) => {
            frame(w as usize).x[0] = 0;
            sched::set_irq_deadline(w as usize, 0);
            sched::wake_if(w as usize, State::IrqWait(idx as u8));
        }
        None => irq.soft = true,
    }
}

/// A task died. Only lines it was blocked on are touched: another driver's interrupt
/// must not be masked or discarded just because an unrelated task crashed.
pub fn forget(tid: u16) {
    for irq in IRQS.lock(R_IRQ).iter_mut().filter(|i| i.used && i.waiter == Some(tid)) {
        irq.waiter = None;
        irq.pending = false;
        irq.soft = false;
        gic::disable(irq.gic_id); // re-enabled by the next owner's IRQ_WAIT
    }
}

/// A timed `IRQ_WAIT` ran out. Taking the waiter registration under the IRQ lock settles the race with a
/// real interrupt: whichever side gets there first completes the wait, the other finds nobody waiting.
pub fn expire(tid: usize, idx: u8) {
    let mut irqs = IRQS.lock(R_IRQ);
    let irq = &mut irqs[idx as usize];
    if irq.waiter == Some(tid as u16) {
        irq.waiter = None;
        frame(tid).x[0] = crate::syscall::E_TIMEDOUT as u64; // the blocked IRQ_WAIT returns the error
        sched::wake_if(tid, State::IrqWait(idx as u8));
    }
}
