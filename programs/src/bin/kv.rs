#![no_std]
#![no_main]
use metal_programs::*;

// A key-value server on the endpoint its parent made and gave it as slot 0 (send+receive): SET(key, value) = 1,
// GET(key) = 2, replies [status, value]. Runs until killed or its endpoint ends.
program!("kv", |_c| {
    let mut keys = [0u64; 64];
    let mut vals = [0u64; 64];
    let mut n = 0usize;
    loop {
        let m = match recv(0) {
            Ok(m) => m,
            Err(_) => return n as u64,
        };
        let at = keys[..n].iter().position(|&k| k == m.w[1]);
        match m.w[0] {
            1 => {
                match at {
                    Some(i) => vals[i] = m.w[2],
                    None if n < 64 => {
                        keys[n] = m.w[1];
                        vals[n] = m.w[2];
                        n += 1;
                    }
                    None => {
                        reply([1, 0, 0]);
                        continue;
                    }
                }
                reply([0, 0, 0]);
            }
            2 => match at {
                Some(i) => {
                    reply([0, vals[i], 0]);
                }
                None => {
                    reply([2, 0, 0]);
                }
            },
            _ => {
                reply([6, 0, 0]);
            }
        }
    }
});
