#![no_std]
#![no_main]
use metallium_programs::*;

// Calls the endpoint its parent delegated as slot 0 (the demo driver: OP_PING answers with a counter) and exits with
// the answer; 1000 + the error if there is no such capability.
program!("ping", |_c| {
    let (st, r) = call(0, [10, 0, 0]);
    if st == 0 {
        r[0]
    } else {
        1000 + (-st) as u64
    }
});
