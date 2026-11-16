//! Polled serial output. The device is the platform's `Uart` (see `hal.rs`); this file adds the console lock and line formatting. Output goes
//! through the console lock, one whole line at a time, so concurrent printing cores never interleave
//! characters within a line.
use crate::hal::Console;
use crate::sync::{SpinLock, R_CONSOLE};
use core::fmt::{self, Write};

static CONSOLE: SpinLock<()> = SpinLock::new(());

fn putc(c: u8) {
    <crate::platform::Uart as Console>::putc(c)
}

struct Raw;

impl fmt::Write for Raw {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                putc(b'\r');
            }
            putc(b);
        }
        Ok(())
    }
}

/// Print one line (a newline is appended) under the console lock.
pub fn print(args: fmt::Arguments<'_>) {
    let _g = CONSOLE.lock(R_CONSOLE);
    let _ = Raw.write_fmt(args);
    let _ = Raw.write_str("\n");
}

/// Write raw user-supplied bytes (the LOG syscall) as one unit.
pub fn write_bytes(bytes: &[u8]) {
    let _g = CONSOLE.lock(R_CONSOLE);
    for &b in bytes {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

/// For panics and fatal errors: no lock (the panicking core may hold any lock).
pub fn panic_print(args: fmt::Arguments<'_>) {
    let _ = Raw.write_fmt(args);
    let _ = Raw.write_str("\n");
}

#[macro_export]
macro_rules! kprintln {
    ($($a:tt)*) => {
        $crate::uart::print(format_args!($($a)*))
    };
}
