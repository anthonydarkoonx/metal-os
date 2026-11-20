#![no_std]
#![no_main]
use metallium_programs::*;

// Memory limits: the argument is the heap quota the parent set (pages). Takes all of it, writes and checks every page,
// then asks for one page more, which must be refused. Exits 0 if everything is as it should be.
program!("mem", |c| {
    let quota = c.arg;
    let base = heap_grow(quota);
    if base <= 0 {
        return 1;
    }
    for p in 0..quota as usize {
        let page = (base as usize + p * 4096) as *mut u64;
        // SAFETY: `base` is the start of `quota` pages just taken with `heap_grow` (checked > 0) and `p < quota`, so `page` is a mapped, writable, page-aligned address of ours.
        unsafe { page.write_volatile(0xA5A5_0000 + p as u64) };
    }
    for p in 0..quota as usize {
        let page = (base as usize + p * 4096) as *const u64;
        // SAFETY: the same page as written above (`p < quota`), still ours and mapped.
        if unsafe { page.read_volatile() } != 0xA5A5_0000 + p as u64 {
            return 2;
        }
    }
    if heap_grow(1) > 0 {
        return 3;
    }
    0
});
