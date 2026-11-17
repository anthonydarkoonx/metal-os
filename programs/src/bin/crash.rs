#![no_std]
#![no_main]
use metal_programs::*;

program!("crash", |_c| {
    println!("[crash] about to touch unmapped memory");
    // SAFETY: deliberately not sound: address 0x10 is never mapped for a program, so this write must fault and get the program killed; that is the test.
    unsafe { core::ptr::write_volatile(0x10 as *mut u64, 1) };
    0
});
