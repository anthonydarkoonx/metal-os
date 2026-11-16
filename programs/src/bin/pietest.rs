#![no_std]
#![no_main]
use core::ptr::read_volatile;
use metal_programs::*;

// Meant to be built position-independent (`make programs`): follows pointers that only work once the loader has relocated
// them, and prints where its code and data really are.
static MARK: u64 = 0x1234_5678;
static PTR: &u64 = &MARK;
fn one() -> u64 {
    1
}
fn two() -> u64 {
    2
}
static FUNCS: [fn() -> u64; 2] = [one, two];

program!("pietest", |c| {
    let via_ptr = *unsafe { read_volatile(&PTR) };
    let f = unsafe { read_volatile(&FUNCS[1]) };
    let ok = via_ptr == 0x1234_5678 && f() == 2 && unsafe { read_volatile(&FUNCS[0]) }() == 1;
    println!("ok={} code={:#x} data={:#x}", ok, f as usize, &MARK as *const u64 as usize);
    c.arg + ok as u64
});
