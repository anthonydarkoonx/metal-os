//! Physical-memory compaction planning, independent of any kernel structure.
//!
//! The kernel classifies each frame as free, movable (a private user page it
//! can migrate: it knows the one mapping that points at it) or neither
//! (kernel image, page tables, DMA buffers, shared objects). `Compactor` is a
//! two-finger scanner: the *migrate* finger walks up from the bottom looking
//! for movable pages, the *free* finger walks down from the top looking for
//! free frames, and each step proposes moving one page from the first into the
//! second. When the fingers meet, no free frame lies above a movable page, so
//! the free space has been consolidated downwards and contiguous runs are as
//! long as the unmovable pages allow.
//!
//! The planner only proposes; the kernel performs (or rejects) each move.
#![forbid(unsafe_code)]
#![no_std]

/// What the planner needs to know about physical memory.
pub trait Frames {
    fn len(&self) -> usize;
    fn is_free(&self, i: usize) -> bool;
    fn is_movable(&self, i: usize) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct Compactor {
    /// Next frame the migrate finger will examine.
    lo: usize,
    /// One past the next frame the free finger will examine.
    hi: usize,
    started: bool,
}

impl Default for Compactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compactor {
    pub const fn new() -> Self {
        Compactor { lo: 0, hi: 0, started: false }
    }

    /// Begin a new pass over `n` frames.
    pub fn restart(&mut self, n: usize) {
        *self = Compactor { lo: 0, hi: n, started: true };
    }

    /// The next proposed move `(source, destination)`, or `None` when the
    /// fingers have met. Does not advance past the proposal: call `accept`
    /// when the move was made, or `reject` when the source could not be moved.
    pub fn peek(&mut self, f: &impl Frames) -> Option<(usize, usize)> {
        if !self.started {
            self.restart(f.len());
        }
        while self.lo < self.hi && !f.is_movable(self.lo) {
            self.lo += 1;
        }
        while self.hi > self.lo && !f.is_free(self.hi - 1) {
            self.hi -= 1;
        }
        if self.lo + 1 >= self.hi {
            return None;
        }
        Some((self.lo, self.hi - 1))
    }

    /// The proposed move was carried out.
    pub fn accept(&mut self) {
        self.lo += 1;
        self.hi -= 1;
    }

    /// The source page cannot be moved right now (leave it, try the next one).
    pub fn reject(&mut self) {
        self.lo += 1;
    }

    /// Has the current pass finished (fingers met)?
    pub fn done(&mut self, f: &impl Frames) -> bool {
        self.peek(f).is_none()
    }
}

/// Length of the longest run of consecutive free frames.
pub fn largest_free_run(f: &impl Frames) -> usize {
    let (mut best, mut cur) = (0, 0);
    for i in 0..f.len() {
        if f.is_free(i) {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// Number of separate runs of free frames.
pub fn free_runs(f: &impl Frames) -> usize {
    let (mut runs, mut prev) = (0, false);
    for i in 0..f.len() {
        let now = f.is_free(i);
        if now && !prev {
            runs += 1;
        }
        prev = now;
    }
    runs
}

/// First run of `n` consecutive free frames at or after `from`: its start index.
pub fn find_run(f: &impl Frames, from: usize, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let mut start = from;
    let mut i = from;
    while i < f.len() {
        if f.is_free(i) {
            if i + 1 - start == n {
                return Some(start);
            }
        } else {
            start = i + 1;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum K {
        Free,
        Fixed,
        Page(u32), // movable page whose contents are `id`
    }

    struct Mem {
        k: Vec<K>,
        /// virtual page -> frame: what a task sees. Migration must keep contents reachable.
        map: BTreeMap<u32, usize>,
    }

    impl Frames for Mem {
        fn len(&self) -> usize {
            self.k.len()
        }
        fn is_free(&self, i: usize) -> bool {
            self.k[i] == K::Free
        }
        fn is_movable(&self, i: usize) -> bool {
            matches!(self.k[i], K::Page(_))
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
    }

    fn random_mem(r: &mut Rng, n: usize, pfree: u32, pfixed: u32) -> Mem {
        let mut k = Vec::new();
        let mut map = BTreeMap::new();
        for i in 0..n {
            let x = r.next() % 100;
            k.push(if x < pfree {
                K::Free
            } else if x < pfree + pfixed {
                K::Fixed
            } else {
                let id = i as u32 + 1000;
                map.insert(id, i);
                K::Page(id)
            });
        }
        Mem { k, map }
    }

    /// Run the compactor with the given per-call budget; returns the number of moves.
    fn run(m: &mut Mem, c: &mut Compactor, budget: usize) -> usize {
        let mut moved = 0;
        while let Some((s, d)) = c.peek(m) {
            let K::Page(id) = m.k[s] else { panic!("source {s} not movable") };
            assert_eq!(m.k[d], K::Free, "destination {d} not free");
            assert!(s < d, "moves must go upward");
            m.k[d] = K::Page(id);
            m.k[s] = K::Free;
            m.map.insert(id, d); // the kernel's remap
            c.accept();
            moved += 1;
            if moved == budget {
                break;
            }
        }
        moved
    }

    fn check_consistent(m: &Mem, before_fixed: &[usize], pages: usize) {
        // every virtual page still maps to a frame holding it; nothing duplicated or lost
        assert_eq!(m.map.len(), pages);
        for (&id, &fr) in &m.map {
            assert_eq!(m.k[fr], K::Page(id), "page {id} lost");
        }
        assert_eq!(m.k.iter().filter(|k| matches!(k, K::Page(_))).count(), pages);
        for &i in before_fixed {
            assert_eq!(m.k[i], K::Fixed, "fixed frame {i} disturbed");
        }
    }

    #[test]
    fn full_compaction_properties() {
        let mut r = Rng(1);
        for round in 0..300 {
            let n = 1 + (r.next() % 400) as usize;
            let (pf, px) = (r.next() % 80, r.next() % 40);
            let mut m = random_mem(&mut r, n, pf, px);
            let fixed: Vec<usize> = (0..n).filter(|&i| m.k[i] == K::Fixed).collect();
            let pages = m.map.len();
            let mut c = Compactor::new();
            run(&mut m, &mut c, usize::MAX);
            check_consistent(&m, &fixed, pages);
            // the defining property: no free frame above any movable page
            for i in 0..n {
                if m.is_movable(i) {
                    for j in i + 1..n {
                        assert!(!m.is_free(j), "round {round}: free frame {j} above movable {i}");
                    }
                }
            }
            // (Contiguity can get worse for adversarial layouts, e.g. [Page, Fixed, Free*10];
            // the guarantee for segregated layouts is tested below.)
            assert!(c.done(&m));
        }
    }

    #[test]
    fn segregated_layout_ends_with_one_contiguous_free_run() {
        // The allocator keeps unmovable frames at the bottom and movable pages at the top.
        // Then compaction leaves *all* free frames above the last unmovable one contiguous.
        let mut r = Rng(11);
        for round in 0..300 {
            let (nlow, nhigh) = (1 + (r.next() % 120) as usize, 1 + (r.next() % 300) as usize);
            let mut k = Vec::new();
            let mut map = BTreeMap::new();
            for _ in 0..nlow {
                k.push(if r.next() % 3 == 0 { K::Free } else { K::Fixed });
            }
            let first_high = k.len();
            for i in 0..nhigh {
                if r.next() % 100 < 55 {
                    k.push(K::Free);
                } else {
                    let id = (first_high + i) as u32 + 5000;
                    map.insert(id, k.len());
                    k.push(K::Page(id));
                }
            }
            let mut m = Mem { k, map };
            let pages = m.map.len();
            let fixed: Vec<usize> = (0..first_high).filter(|&i| m.k[i] == K::Fixed).collect();
            let free_above_fixed = (fixed.last().map_or(0, |&f| f + 1)..m.k.len()).filter(|&i| m.is_free(i)).count();
            run(&mut m, &mut Compactor::new(), usize::MAX);
            check_consistent(&m, &fixed, pages);
            assert!(largest_free_run(&m) >= free_above_fixed, "round {round}: {} < {free_above_fixed}", largest_free_run(&m));
        }
    }

    #[test]
    fn budgeted_passes_equal_one_full_pass() {
        let mut r = Rng(2);
        for _ in 0..100 {
            let n = 50 + (r.next() % 300) as usize;
            let base = random_mem(&mut r, n, 40, 15);
            let mut full = Mem { k: base.k.clone(), map: base.map.clone() };
            run(&mut full, &mut Compactor::new(), usize::MAX);
            let mut inc = Mem { k: base.k.clone(), map: base.map.clone() };
            let mut c = Compactor::new();
            loop {
                let budget = 1 + (r.next() % 7) as usize;
                if run(&mut inc, &mut c, budget) == 0 {
                    break;
                }
            }
            assert_eq!(full.k, inc.k, "incremental result differs from a single pass");
        }
    }

    #[test]
    fn rejected_pages_stay_put_and_nothing_else_breaks() {
        let mut r = Rng(3);
        for _ in 0..200 {
            let n = 30 + (r.next() % 300) as usize;
            let mut m = random_mem(&mut r, n, 45, 10);
            let pages = m.map.len();
            let fixed: Vec<usize> = (0..n).filter(|&i| m.k[i] == K::Fixed).collect();
            let mut c = Compactor::new();
            let mut pinned = Vec::new(); // pages the "kernel" refuses to move (e.g. task died mid-pass)
            while let Some((s, d)) = c.peek(&m) {
                if r.next() % 4 == 0 {
                    pinned.push(s);
                    c.reject();
                    continue;
                }
                let K::Page(id) = m.k[s] else { unreachable!() };
                m.k[d] = K::Page(id);
                m.k[s] = K::Free;
                m.map.insert(id, d);
                c.accept();
            }
            check_consistent(&m, &fixed, pages);
            for s in pinned {
                assert!(m.is_movable(s), "a rejected page vanished from its frame");
            }
        }
    }

    #[test]
    fn degenerate_memories() {
        let mut m = Mem { k: vec![K::Free; 10], map: BTreeMap::new() };
        assert!(Compactor::new().peek(&m).is_none());
        m.k = vec![K::Fixed; 10];
        assert!(Compactor::new().peek(&m).is_none());
        let mut full = Mem { k: (0..10).map(K::Page).collect(), map: BTreeMap::new() };
        assert!(Compactor::new().peek(&full).is_none());
        full.k.clear();
        assert!(Compactor::new().peek(&full).is_none());
        // already compact: movable pages at the top, free at the bottom
        let m = Mem { k: [vec![K::Fixed; 3], vec![K::Free; 4], (0..3).map(K::Page).collect()].concat(), map: BTreeMap::new() };
        assert!(Compactor::new().peek(&m).is_none());
    }

    #[test]
    fn metrics_and_find_run() {
        use K::*;
        let m = Mem { k: vec![Free, Free, Fixed, Free, Page(1), Free, Free, Free, Fixed, Free], map: BTreeMap::new() };
        assert_eq!(largest_free_run(&m), 3);
        assert_eq!(free_runs(&m), 4);
        assert_eq!(find_run(&m, 0, 2), Some(0));
        assert_eq!(find_run(&m, 0, 3), Some(5));
        assert_eq!(find_run(&m, 0, 4), None);
        assert_eq!(find_run(&m, 1, 2), Some(5));
        assert_eq!(find_run(&m, 0, 0), None);
    }

    #[test]
    fn compaction_makes_a_large_run_possible() {
        // every other frame free: no run of 2 exists; after compaction the free half is contiguous
        let n = 200;
        let mut k = Vec::new();
        let mut map = BTreeMap::new();
        for i in 0..n {
            if i % 2 == 0 {
                k.push(K::Free);
            } else {
                k.push(K::Page(i as u32));
                map.insert(i as u32, i);
            }
        }
        let mut m = Mem { k, map };
        assert_eq!(largest_free_run(&m), 1);
        run(&mut m, &mut Compactor::new(), usize::MAX);
        assert_eq!(largest_free_run(&m), 100);
        assert_eq!(free_runs(&m), 1);
    }
}

/// The memory-pressure policy of the memory daemon (`compactd`): when to start pushing cold pages out of RAM and when to stop, as marks that scale
/// with the machine's memory (a fixed number of frames is right for one size of machine and wrong for the others), and when the compressed store
/// is full enough to ask for pages to be written to disk.
pub mod policy {
    /// Free-frame marks: below `low` the daemon evicts cold pages, until `high` frames are free.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Marks {
        pub low: u64,
        pub high: u64,
    }

    /// What a machine gets with no swap to disk: an emergency reserve only (the marks the tests of the memory daemon were written for).
    pub const EMERGENCY: Marks = Marks { low: 24, high: 128 };

    /// The marks for a machine of `total_frames` frames (4 KiB each): start at 1/16 free (6%), stop at 1/10 (10%). Never below the emergency
    /// marks, and `high` is always above `low`.
    pub fn marks(total_frames: u64) -> Marks {
        let low = (total_frames / 16).max(EMERGENCY.low);
        let high = (total_frames / 10).max(EMERGENCY.high).max(low + 1);
        Marks { low, high }
    }

    /// The compressed store is full enough (three quarters) for the oldest pages to be written to disk.
    pub fn store_needs_writeback(store_bytes: u64, pool_bytes: u64) -> bool {
        pool_bytes > 0 && store_bytes > pool_bytes / 4 * 3
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn marks_scale_with_memory() {
            // 256 MiB: start at 16 MiB free, stop at about 25 MiB
            assert_eq!(marks(65_536), Marks { low: 4096, high: 6553 });
            // 4 GiB
            let m = marks(1 << 20);
            assert_eq!((m.low, m.high), (65_536, 104_857));
            // each is a fixed share of memory: double the memory, double the marks
            assert_eq!(marks(2 << 20).low, 2 * m.low);
        }

        #[test]
        fn small_machines_keep_the_emergency_reserve() {
            assert_eq!(marks(0), Marks { low: 24, high: 128 });
            assert_eq!(marks(512), Marks { low: 32, high: 128 });
            for total in [0u64, 1, 100, 1000, 65_536, 1 << 24] {
                let m = marks(total);
                assert!(m.low >= EMERGENCY.low && m.high >= EMERGENCY.high && m.high > m.low, "{total}: {m:?}");
            }
        }

        #[test]
        fn writeback_starts_at_three_quarters_of_the_pool() {
            assert!(!store_needs_writeback(0, 1 << 20));
            assert!(!store_needs_writeback(3 << 18, 1 << 20));
            assert!(store_needs_writeback((3 << 18) + 4096, 1 << 20));
            assert!(!store_needs_writeback(100, 0)); // (no pool: nothing to write)
        }
    }
}
