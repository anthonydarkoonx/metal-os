//! The contract between the shared kernel and a machine's early hardware.
//!
//! The kernel needs a few devices before any user-space driver can exist: a console, the interrupt controller, a way to start cores and power the machine
//! off, and a clock. Each machine supplies one type per trait (QEMU `virt`: `platform/qemu_virt.rs`) and names it in its `platform` module; the shared files
//! (`uart.rs`, `gic.rs`, `smp.rs`, ...) call the traits through those types.
//!
//! Every method is an associated function of a zero-sized type, so the choice is made at compile time: the compiler produces the same code as for a
//! hand-written driver file, with no table and no indirect call.
//!
//! Everything else a machine differs in (the memory map, the link address, the devices it probes, the capabilities it gives boot tasks) stays a constant
//! or a hook in `platform.rs`; see docs/PORTING.md.

/// The polled serial console. Locking and formatting are the shared `uart.rs`'s job; a machine only moves one byte.
pub trait Console {
    /// Send one byte, waiting for the transmitter if it must. Called under the console lock (and without it from a panic), possibly on any core.
    fn putc(b: u8);
}

/// The interrupt controller (GIC on every machine so far, in whichever version). Interrupt ids are the controller's own numbers.
pub trait IrqController {
    /// The distributor: once, on the boot core.
    fn init_dist();
    /// This core's interface, and the banked interrupts (software-generated ones and the timer): on every core.
    fn init_cpu();
    /// Inter-processor interrupt `id` to exactly the cores in `mask` (bit n = core n).
    fn send_sgi_to(mask: u32, id: u32);
    fn enable(id: u32);
    fn disable(id: u32);
    /// Route a shared interrupt to core 0 at the default device priority; the line stays disabled.
    fn configure_spi(id: u32);
    /// Raise an interrupt above the default device priority (smaller number = more urgent).
    fn set_urgent(id: u32);
    /// Make a shared interrupt edge-triggered.
    fn set_edge(id: u32);
    /// Acknowledge the highest-priority pending interrupt: the value to give back to `eoi` (its low bits are the interrupt id; 1020 and up: none).
    fn ack() -> u32;
    fn eoi(iar: u32);
}

/// Power and core start-up (PSCI on the machines so far; the conduit, HVC or SMC, is the machine's).
pub trait PowerControl {
    /// Switch the machine off (`reset` false) or restart it. Returns only if the firmware refused.
    fn power(reset: bool);
    /// Start `core` at `entry` with `core` as its context id; the status the firmware returned (0 = started).
    ///
    /// # Safety
    /// `entry` must be the secondary core's entry point.
    unsafe fn cpu_on(core: usize, entry: u64) -> i64;
}

/// The wall clock, if the machine has one.
pub trait Rtc {
    /// Seconds since 1970, or `None` when there is no clock or it is not set.
    fn seconds() -> Option<u64>;
}
