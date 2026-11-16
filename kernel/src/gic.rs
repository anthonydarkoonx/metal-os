//! The interrupt controller. The device is the platform's `Intc` (see `hal.rs`); these are the names the rest of the kernel uses, and they compile
//! to the driver's own code.
use crate::hal::IrqController;
use crate::platform::Intc;

/// Distributor: once, on the boot core.
#[inline]
pub fn init_dist() {
    Intc::init_dist()
}

/// CPU interface and banked PPI/SGI enables: on every core.
#[inline]
pub fn init_cpu() {
    Intc::init_cpu()
}

/// Inter-processor interrupt: SGI `id` to exactly the cores in `mask`.
#[inline]
pub fn send_sgi_to(mask: u32, id: u32) {
    Intc::send_sgi_to(mask, id)
}

#[inline]
pub fn enable(id: u32) {
    Intc::enable(id)
}

#[inline]
pub fn disable(id: u32) {
    Intc::disable(id)
}

/// Route an SPI to CPU 0 at a fixed priority; the line stays disabled.
#[inline]
pub fn configure_spi(id: u32) {
    Intc::configure_spi(id)
}

/// Raise an interrupt above the default device priority (smaller number = more urgent), so when
/// it is pending together with a device's interrupt it is taken first.
#[inline]
pub fn set_urgent(id: u32) {
    Intc::set_urgent(id)
}

/// Make an SPI edge-triggered.
#[inline]
pub fn set_edge(id: u32) {
    Intc::set_edge(id)
}

#[inline]
pub fn ack() -> u32 {
    Intc::ack()
}

#[inline]
pub fn eoi(iar: u32) {
    Intc::eoi(iar)
}
