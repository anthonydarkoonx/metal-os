#![no_std]
#![no_main]
extern crate alloc;
use alloc::vec::Vec;
use metallium_programs::ability::*;
use metallium_programs::*;

// Sample program that eats memory until its limit stops it, every time it is started.
struct Hog;

impl Ability for Hog {
    fn on_create(&mut self, _r: Option<(u64, u64)>) {
        let mut keep: Vec<Vec<u8>> = Vec::new();
        loop {
            keep.push(alloc::vec![1u8; 8192]);
        }
    }
}

ability!("hog", |_c| Hog);
