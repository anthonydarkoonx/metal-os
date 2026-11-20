#![no_std]
#![no_main]
use metallium_programs::ability::*;
use metallium_programs::*;

// Sample program "counter": counts ticks and records which lifecycle callbacks it has seen
// (saved word 1: create 1, foreground 2, background 4, destroy 8; saved word 0: the count).
struct Counter {
    n: u64,
    seen: u64,
}

impl Counter {
    fn keep(&self) {
        save(self.n, self.seen);
    }
}

impl Ability for Counter {
    fn on_create(&mut self, restored: Option<(u64, u64)>) {
        if let Some((n, s)) = restored {
            self.n = n;
            self.seen = s;
        }
        self.seen |= 1;
        self.keep();
    }
    fn on_foreground(&mut self) {
        self.seen |= 2;
        self.keep();
    }
    fn on_background(&mut self) {
        self.seen |= 4;
        self.keep();
    }
    fn on_destroy(&mut self) {
        self.seen |= 8;
        self.keep();
    }
    fn on_tick(&mut self) {
        self.n += 1;
        self.keep();
    }
    fn tick_interval(&self) -> u64 {
        5
    }
}

ability!("counter", |_c| Counter { n: 0, seen: 0 });
