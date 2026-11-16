#![no_std]
#![no_main]
use metal_programs::*;

// kv client: slot 0 is a send capability to the server. Stores `arg` keys (scaled by its argument so that clients do not
// overlap), reads them back; exits 0 if everything matched, else 1 + the index that did not (or 5000 + the IPC error).
program!("kvc", |c| {
    let base = c.arg * 1000;
    for i in 0..20u64 {
        let (st, r) = call(0, [1, base + i, (base + i) * 7]);
        if st != 0 {
            return 5000 + (-st) as u64;
        }
        if r[0] != 0 {
            return 1 + i;
        }
    }
    for i in 0..20u64 {
        let (st, r) = call(0, [2, base + i, 0]);
        if st != 0 {
            return 5000 + (-st) as u64;
        }
        if r[0] != 0 || r[1] != (base + i) * 7 {
            return 1 + i;
        }
    }
    0
});
