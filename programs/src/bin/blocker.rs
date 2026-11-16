#![no_std]
#![no_main]
use metal_programs::*;

// Calls the endpoint in slot 0 and exits with 1000 + the error (or 0 if it was answered). With nobody serving, this
// blocks until the endpoint ends, which must return -4.
program!("blocker", |_c| {
    let (st, _) = call(0, [1, 0, 0]);
    if st == 0 {
        0
    } else {
        1000 + (-st) as u64
    }
});
