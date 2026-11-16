#![no_std]
#![no_main]
use metal_programs::*;

// Computes for `arg` ticks of wall time, then exits with a checksum (so that the loop is not optimised away).
program!("burn", |c| {
    let end = ticks() + c.arg;
    let mut x = 1u64;
    while ticks() < end {
        for i in 0..5000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
    }
    x & 0xFFFF
});
