#![no_std]
#![no_main]
use metal_programs::*;

// Lives until it is killed (or its parent goes away).
program!("sleeper", |_c| -> u64 { loop {
    sleep(10);
} });
