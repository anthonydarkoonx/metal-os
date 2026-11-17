#![no_std]
#![no_main]
use metal_programs::ability::*;
use metal_programs::*;

// Sample program that crashes once: its first instance touches unmapped memory after 10 ticks; the instance the manager
// starts afterwards is given the count and runs on (saved word 1 is 1 for the first instance, 2 for the next).
struct Crasher {
    n: u64,
    restored: bool,
}

impl Ability for Crasher {
    fn on_create(&mut self, restored: Option<(u64, u64)>) {
        if let Some((n, _)) = restored {
            self.n = n;
            self.restored = true;
        }
        save(self.n, if self.restored { 2 } else { 1 });
    }
    fn on_tick(&mut self) {
        self.n += 1;
        save(self.n, if self.restored { 2 } else { 1 });
        if !self.restored && self.n == 10 {
            // SAFETY: deliberately not sound: address 0x10 is never mapped for a program, so this write must fault and make the manager restart the program; that is the test.
            unsafe { core::ptr::write_volatile(0x10 as *mut u64, 1) };
        }
    }
    fn tick_interval(&self) -> u64 {
        3
    }
}

ability!("crasher", |_c| Crasher { n: 0, restored: false });
