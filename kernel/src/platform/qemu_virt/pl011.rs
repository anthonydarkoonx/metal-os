//! QEMU `virt`: the PL011 serial port at 0x0900_0000, polled, no set-up needed.
use crate::hal::Console;
use core::ptr::{read_volatile, write_volatile};

const UART0: *mut u32 = 0x0900_0000 as *mut u32;

pub struct Pl011;

impl Console for Pl011 {
    fn putc(c: u8) {
        unsafe {
            while read_volatile(UART0.add(6)) & (1 << 5) != 0 {} // FR.TXFF
            write_volatile(UART0, c as u32);
        }
    }
}
