#![no_std]
#![no_main]
use metal_programs::*;

// Lives until it is killed (or its parent goes away). With argument 1 and an endpoint factory in slot 1 it first makes what a program
// with several endpoints and shared-memory objects holds (4 endpoints and 2 objects, `make scale`); if that is refused it ends
// with 100 + how many it got.
program!("sleeper", |c| -> u64 {
    if c.arg == 1 {
        let mut made = 0;
        for _ in 0..4 {
            if ep_create(1) >= 0 {
                made += 1;
            }
        }
        for _ in 0..2 {
            if shm_create(1) >= 0 {
                made += 1;
            }
        }
        if made != 6 {
            return 100 + made;
        }
    }
    loop {
        sleep(10);
    }
});
