#![no_std]
#![no_main]
use metal_programs::*;

// The victim of the pager tests (user/src/programs.rs): keeps a known pattern in heap pages while the conductor pushes the
// pages out to the compressed store and on to disk, then checks that every byte comes back.
//   0  fill, sleep (the conductor swaps and writes back), check a system call on a page that is on disk, check all pages
//   1  keep reading and rewriting the pages for a while, so that some are touched while their write is in flight
const PAGE: usize = 4096;
const N: usize = 48;

fn word(p: usize, w: usize, gen: u64) -> u64 {
    ((p as u64) << 40 | gen << 32 | w as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5851_F42D
}

fn fill(base: usize, p: usize, gen: u64) {
    for w in 0..PAGE / 8 {
        // UNVERIFIED: `fill` is a safe fn that writes through a caller-supplied integer address; it is sound only because every caller passes `base` from `heap_grow(N)` and `p < N` (w < PAGE / 8 keeps the 8-aligned word inside the page).
        unsafe { core::ptr::write_volatile((base + p * PAGE + w * 8) as *mut u64, word(p, w, gen)) };
    }
}

fn check(base: usize, p: usize, gen: u64) -> bool {
    // UNVERIFIED: `check` is a safe fn that reads through a caller-supplied integer address; sound only because every caller passes `base` from `heap_grow(N)` and `p < N`.
    (0..PAGE / 8).all(|w| unsafe { core::ptr::read_volatile((base + p * PAGE + w * 8) as *const u64) } == word(p, w, gen))
}

program!("pagetest", |c| -> u64 {
    let base = heap_grow(N as u64);
    if base < 0 {
        return 100;
    }
    let base = base as usize;
    let mut gens = [0u64; N];
    for p in 0..N {
        fill(base, p, 0);
    }
    let msg = b"[pagetest] this line was read from a page that was on disk\n";
    let at = base + 3 * PAGE + 100;
    // (the message sits in page 3, after its pattern is written: keep the pattern valid by writing it past the words)
    for (i, &b) in msg.iter().enumerate() {
        // SAFETY: `at + i` is inside page 3 of the `N` pages from `heap_grow` (`at` = base + 3 pages + 100, and the message is under 100 bytes).
        unsafe { core::ptr::write_volatile((at + i) as *mut u8, b) };
    }
    match c.arg {
        0 => {
            println!("[pagetest] filled");
            // wait for the conductor (it has pushed the pages out to the disk) to create the file that says so
            let give_up = ticks() + 3000;
            while !metal_programs::fs::exists("/data/pagetest.go") {
                if ticks() > give_up {
                    println!("[pagetest] nobody came");
                    return 201;
                }
                sleep(5);
            }
            let r = svc(LOG, CONSOLE, at as u64, msg.len() as u64, 0, 0)[0] as i64;
            if r != msg.len() as i64 {
                println!("[pagetest] LOG on a page on disk returned {r}");
                return 200;
            }
            let bad = (0..N).filter(|&p| p != 3 && !check(base, p, 0)).count();
            // page 3 holds the message in the middle of its pattern: compare around it
            // SAFETY: page 3 is one of the `N` pages from `heap_grow` and `w < PAGE / 8`, so each 8-aligned word read is inside it.
            let page3 = (0..PAGE / 8).filter(|&w| (w * 8 + 8 <= 100 || w * 8 >= 100 + msg.len() + 7) && unsafe { core::ptr::read_volatile((base + 3 * PAGE + w * 8) as *const u64) } != word(3, w, 0)).count();
            println!("[pagetest] checked: {bad} pages wrong, {page3} words wrong in the page with the message");
            (bad + page3).min(99) as u64
        }
        _ => {
            let end = ticks() + 400;
            let mut x = 0x1234_5678_9ABCu64;
            let mut rounds = 0u64;
            let mut bad = 0;
            while ticks() < end {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let p = (x % N as u64) as usize;
                if p == 3 {
                    continue;
                }
                if !check(base, p, gens[p]) {
                    bad += 1;
                }
                gens[p] += 1;
                fill(base, p, gens[p]);
                rounds += 1;
                sleep(1); // (a page touched at once after being pushed out never gets as far as the disk)
            }
            let after = (0..N).filter(|&p| p != 3 && !check(base, p, gens[p])).count();
            println!("[pagetest] {rounds} rewrites, {bad} wrong while running, {after} wrong at the end");
            (bad + after).min(99) as u64
        }
    }
});
