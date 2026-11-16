#![no_std]
#![no_main]
use metal_programs::*;

static SLEEPER: &[u8] = include_bytes!("../../build/sleeper.bin");

// Holds the spawn capability as slot 0. Starts two sleepers and leaves without waiting: the kernel must end them. Exits
// with their task ids packed (first | second << 8), or 0xFFFF_FFFF - the error.
program!("spawner", |c| {
    if c.arg == 1 {
        // asks for a child with file-system access, which its own spawn capability may not grant
        let mut p = Spawn::new(SLEEPER);
        p.fs = true;
        return 0xFFFF_FFFF - (-spawn(0, &p)) as u64;
    }
    let a = spawn(0, &Spawn::new(SLEEPER));
    let b = spawn(0, &Spawn::new(SLEEPER));
    if a < 0 || b < 0 {
        return 0xFFFF_FFFF - (-(if a < 0 { a } else { b })) as u64;
    }
    a as u64 | (b as u64) << 8
});
