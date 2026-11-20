#![no_std]
#![no_main]
use core::ptr::read_volatile;
use metallium_programs::*;

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
    // SAFETY (the three reads below): `&PTR`, `&FUNCS[i]` are references to live statics, so the pointers are valid and aligned; volatile only keeps the compiler from folding the relocated values.
    // SAFETY: `&PTR` is a reference to a live static, so the pointer is valid and aligned; volatile only keeps the compiler from folding the relocated value.
    let via_ptr = *unsafe { read_volatile(&PTR) };
    // SAFETY: `&FUNCS[1]` is a reference to a live static, so the pointer is valid and aligned.
    let f = unsafe { read_volatile(&FUNCS[1]) };
    // SAFETY: `&FUNCS[0]` is a reference to a live static, so the pointer is valid and aligned.
    let ok = via_ptr == 0x1234_5678 && f() == 2 && unsafe { read_volatile(&FUNCS[0]) }() == 1;
    println!("ok={} code={:#x} data={:#x}", ok, f as usize, &MARK as *const u64 as usize);
    c.arg + ok as u64
});
