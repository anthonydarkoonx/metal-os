#![no_std]
#![no_main]
use metal_programs::*;

program!("crash", |_c| {
    println!("[crash] about to touch unmapped memory");
    unsafe { core::ptr::write_volatile(0x10 as *mut u64, 1) };
    0
});
