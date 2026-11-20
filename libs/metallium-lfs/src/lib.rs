//! Log-structured copy-on-write block store (F2FS/HMFS-inspired, not
//! on-disk compatible). Random writes are buffered in RAM, coalesced, and
//! flushed sequentially to fresh blocks; the key->block index is persisted as
//! incremental delta records, and an atomic double-buffered checkpoint makes
//! each `sync()` crash-consistent.
//!
//! The log area is divided into fixed-size *segments*. Writes fill one
//! segment sequentially, then move to the next free segment. A segment is free
//! when no block in it is referenced by the last committed checkpoint
//! (live data blocks or live index-delta blocks). When free space runs low,
//! the cleaner (garbage collector) copies the live blocks out of the segment
//! with the least live data, rewrites the index as a fresh snapshot, and
//! commits; the victim is then free again. Space freed by a commit is only
//! reused by a *later* commit, so a crash at any point still finds the last
//! checkpoint's blocks intact.
//!
//! Not handled: IO errors in the middle of a commit leave in-memory
//! accounting stale (remount to recover); there is no wear-leveling beyond
//! round-robin segment allocation.
#![forbid(unsafe_code)]
#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

pub const BS: usize = 4096;
const CP_MAGIC: u32 = 0x4D43_4B50; // "MCKP"
const DELTA_MAGIC: u32 = 0x4D44_4C54; // "MDLT"
const PER_DELTA: usize = (BS - 16 - 4) / 16;
/// Rewrite the whole index (instead of appending a delta) past this chain length.
const MAX_CHAIN: usize = 32;
const FIRST_LOG_BLOCK: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Io,
    NoSpace,
    Corrupt,
    NotFound,
}

pub trait BlockDev {
    fn blocks(&self) -> u64;
    fn read(&self, blk: u64, buf: &mut [u8; BS]) -> Result<(), Error>;
    fn write(&mut self, blk: u64, buf: &[u8; BS]) -> Result<(), Error>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Calls to `write_block`.
    pub logical_writes: u64,
    /// Blocks actually written to the device (data + index + checkpoints).
    pub device_writes: u64,
    /// Cleaner passes (one victim segment each).
    pub gc_runs: u64,
    /// Live data blocks the cleaner had to copy.
    pub gc_moved: u64,
}

pub struct Lfs<D> {
    dev: D,
    index: BTreeMap<u64, u64>,
    staged: BTreeMap<u64, Option<Box<[u8; BS]>>>,
    seq: u64,
    seg_blocks: u64,
    nsegs: usize,
    /// Next block to write inside `cur_seg`.
    head: u64,
    cur_seg: Option<usize>,
    /// Live blocks per segment (committed state plus blocks written by the commit in progress).
    live: Vec<u32>,
    /// Live delta blocks of the index chain, oldest first.
    chain: Vec<u64>,
    force_snapshot: bool,
    pub stats: Stats,
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(c & 1));
        }
    }
    !c
}

fn seal(buf: &mut [u8; BS]) {
    let c = crc32(&buf[..BS - 4]);
    buf[BS - 4..].copy_from_slice(&c.to_le_bytes());
}

fn sealed_ok(buf: &[u8; BS]) -> bool {
    crc32(&buf[..BS - 4]) == u32::from_le_bytes(buf[BS - 4..].try_into().unwrap())
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Segment size is a pure function of the device size, so mount recomputes it.
fn seg_blocks_for(blocks: u64) -> u64 {
    (blocks / 32).clamp(4, 32)
}

impl<D: BlockDev> Lfs<D> {
    fn new(dev: D) -> Self {
        let blocks = dev.blocks();
        let seg_blocks = seg_blocks_for(blocks);
        let nsegs = (blocks.saturating_sub(FIRST_LOG_BLOCK) / seg_blocks) as usize;
        Lfs {
            dev,
            index: BTreeMap::new(),
            staged: BTreeMap::new(),
            seq: 0,
            seg_blocks,
            nsegs,
            head: 0,
            cur_seg: None,
            live: vec![0; nsegs],
            chain: Vec::new(),
            force_snapshot: false,
            stats: Stats::default(),
        }
    }

    fn seg_start(&self, s: usize) -> u64 {
        FIRST_LOG_BLOCK + s as u64 * self.seg_blocks
    }

    fn seg_of(&self, pba: u64) -> usize {
        ((pba - FIRST_LOG_BLOCK) / self.seg_blocks) as usize
    }

    /// Create an empty filesystem (destroys existing contents).
    pub fn format(dev: D) -> Result<Self, Error> {
        if dev.blocks() < FIRST_LOG_BLOCK + 1 {
            return Err(Error::NoSpace);
        }
        let mut fs = Lfs::new(dev);
        fs.write_checkpoint(0)?;
        Ok(fs)
    }

    pub fn mount(dev: D) -> Result<Self, Error> {
        let mut best: Option<(u64, [u8; BS])> = None;
        for slot in 0..2 {
            let mut b = [0u8; BS];
            match dev.read(slot, &mut b) {
                Ok(()) => {}
                Err(Error::Corrupt) => continue, // (a torn or blank block of an encrypted volume: the other checkpoint may be good)
                Err(e) => return Err(e),
            }
            if rd32(&b, 0) == CP_MAGIC && sealed_ok(&b) {
                let seq = rd64(&b, 4);
                if best.as_ref().map_or(true, |(s, _)| seq > *s) {
                    best = Some((seq, b));
                }
            }
        }
        let (seq, cp) = best.ok_or(Error::Corrupt)?;
        let head = rd64(&cp, 12);
        let last_delta = rd64(&cp, 20);
        let mut fs = Lfs::new(dev);
        fs.seq = seq;
        // Walk the delta chain newest -> oldest, then replay oldest -> newest.
        let mut blocks = Vec::new();
        let mut at = last_delta;
        while at != 0 {
            if at < FIRST_LOG_BLOCK || at >= fs.dev.blocks() {
                return Err(Error::Corrupt);
            }
            let mut b = Box::new([0u8; BS]);
            fs.dev.read(at, &mut b)?;
            if rd32(&b[..], 0) != DELTA_MAGIC || !sealed_ok(&b) {
                return Err(Error::Corrupt);
            }
            blocks.push((at, b));
            at = rd64(&blocks.last().unwrap().1[..], 4);
        }
        for (pba, b) in blocks.iter().rev() {
            let n = rd32(&b[..], 12) as usize;
            if n > PER_DELTA {
                return Err(Error::Corrupt);
            }
            for i in 0..n {
                let o = 16 + i * 16;
                let (k, p) = (rd64(&b[..], o), rd64(&b[..], o + 8));
                if p == 0 {
                    fs.index.remove(&k);
                } else if p < FIRST_LOG_BLOCK || p >= fs.dev.blocks() {
                    return Err(Error::Corrupt);
                } else {
                    fs.index.insert(k, p);
                }
            }
            fs.chain.push(*pba);
        }
        // Rebuild per-segment liveness from the recovered state.
        let pbas: Vec<u64> = fs.index.values().copied().chain(fs.chain.iter().copied()).collect();
        for p in pbas {
            let s = fs.seg_of(p);
            if s >= fs.nsegs {
                return Err(Error::Corrupt);
            }
            fs.live[s] += 1;
        }
        if head >= FIRST_LOG_BLOCK && (head - FIRST_LOG_BLOCK) % fs.seg_blocks != 0 {
            let s = fs.seg_of(head);
            if s < fs.nsegs {
                fs.cur_seg = Some(s);
                fs.head = head;
            }
        }
        Ok(fs)
    }

    pub fn into_device(self) -> D {
        self.dev
    }

    /// Stage a block write. Overwriting a staged block costs no device I/O.
    pub fn write_block(&mut self, key: u64, data: &[u8; BS]) {
        self.stats.logical_writes += 1;
        self.staged.insert(key, Some(Box::new(*data)));
    }

    pub fn delete_block(&mut self, key: u64) {
        self.staged.insert(key, None);
    }

    pub fn read_block(&self, key: u64, out: &mut [u8; BS]) -> Result<(), Error> {
        match self.staged.get(&key) {
            Some(Some(d)) => {
                out.copy_from_slice(&d[..]);
                Ok(())
            }
            Some(None) => Err(Error::NotFound),
            None => {
                let pba = *self.index.get(&key).ok_or(Error::NotFound)?;
                self.dev.read(pba, out)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Blocks that can still be written without cleaning (free segments plus
    /// the unused tail of the current one).
    pub fn free_blocks(&self) -> u64 {
        let tail = self.cur_seg.map_or(0, |s| self.seg_start(s) + self.seg_blocks - self.head);
        let free = (0..self.nsegs).filter(|&s| self.live[s] == 0 && Some(s) != self.cur_seg).count() as u64;
        tail + free * self.seg_blocks
    }

    fn alloc(&mut self) -> Result<u64, Error> {
        if let Some(s) = self.cur_seg {
            if self.head < self.seg_start(s) + self.seg_blocks {
                let b = self.head;
                self.head += 1;
                self.live[s] += 1;
                return Ok(b);
            }
        }
        // Next free segment after the current one (round-robin spreads the wear).
        let start = self.cur_seg.map_or(0, |s| s + 1);
        for k in 0..self.nsegs {
            let s = (start + k) % self.nsegs;
            if self.live[s] == 0 && Some(s) != self.cur_seg {
                self.cur_seg = Some(s);
                self.head = self.seg_start(s) + 1;
                self.live[s] += 1;
                return Ok(self.seg_start(s));
            }
        }
        Err(Error::NoSpace)
    }

    fn dev_write(&mut self, blk: u64, buf: &[u8; BS]) -> Result<(), Error> {
        self.dev.write(blk, buf)?;
        self.stats.device_writes += 1;
        Ok(())
    }

    /// Upper bound of the blocks a commit of `staged` would write.
    fn commit_cost(&self, staged: &BTreeMap<u64, Option<Box<[u8; BS]>>>) -> u64 {
        let n_data = staged.values().filter(|v| v.is_some()).count();
        let snapshot = self.force_snapshot || self.chain.len() >= MAX_CHAIN;
        let entries = if snapshot { self.index.len() + staged.len() } else { staged.len() };
        (n_data + entries.div_ceil(PER_DELTA)) as u64
    }

    /// Space a normal commit must leave untouched so the cleaner can always run.
    fn reserve(&self) -> u64 {
        self.seg_blocks + (self.index.len() / PER_DELTA + 2) as u64
    }

    /// Flush staged writes sequentially and publish a new checkpoint,
    /// cleaning segments first if free space is low.
    pub fn sync(&mut self) -> Result<(), Error> {
        if self.staged.is_empty() && !self.force_snapshot {
            return Ok(());
        }
        let user = core::mem::take(&mut self.staged);
        let need = self.commit_cost(&user);
        let mut passes = 0;
        while self.free_blocks() < need + self.reserve() && passes < 2 * self.nsegs + 2 {
            passes += 1;
            match self.clean_one() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    self.merge_staged(user);
                    return Err(e);
                }
            }
        }
        self.merge_staged(user);
        self.commit()
    }

    fn merge_staged(&mut self, user: BTreeMap<u64, Option<Box<[u8; BS]>>>) {
        for (k, v) in user {
            self.staged.insert(k, v); // user writes are newer than anything the cleaner staged
        }
    }

    /// Run one cleaner pass if there is anything worth cleaning.
    pub fn gc(&mut self) -> Result<bool, Error> {
        let user = core::mem::take(&mut self.staged);
        let r = self.clean_one();
        self.merge_staged(user);
        r
    }

    /// Clean the segment with the least live data: copy its live blocks
    /// forward, snapshot the index (retiring old delta blocks), commit.
    fn clean_one(&mut self) -> Result<bool, Error> {
        let seg_blocks = self.seg_blocks as u32;
        let victim = (0..self.nsegs)
            .filter(|&s| Some(s) != self.cur_seg && self.live[s] > 0 && self.live[s] < seg_blocks)
            .min_by_key(|&s| self.live[s]);
        let Some(victim) = victim else { return Ok(false) };
        let moves: Vec<(u64, u64)> =
            self.index.iter().filter(|(_, &p)| self.seg_of(p) == victim).map(|(&k, &p)| (k, p)).collect();
        for (k, pba) in moves {
            let mut b = Box::new([0u8; BS]);
            self.dev.read(pba, &mut b)?;
            self.staged.insert(k, Some(b));
            self.stats.gc_moved += 1;
        }
        self.force_snapshot = true;
        self.commit()?;
        self.stats.gc_runs += 1;
        Ok(true)
    }

    fn commit(&mut self) -> Result<(), Error> {
        if self.free_blocks() < self.commit_cost(&self.staged) {
            return Err(Error::NoSpace);
        }
        let staged = core::mem::take(&mut self.staged);
        let mut dead: Vec<u64> = Vec::new(); // blocks no longer referenced once this commit lands
        let mut delta: Vec<(u64, u64)> = Vec::new();
        for (key, val) in staged {
            match val {
                Some(data) => {
                    let pba = self.alloc()?;
                    self.dev_write(pba, &data)?;
                    delta.push((key, pba));
                    dead.extend(self.index.insert(key, pba));
                }
                None => {
                    delta.push((key, 0));
                    dead.extend(self.index.remove(&key));
                }
            }
        }
        let mut last_delta = self.chain.last().copied().unwrap_or(0);
        if self.force_snapshot || self.chain.len() >= MAX_CHAIN {
            delta = self.index.iter().map(|(&k, &p)| (k, p)).collect();
            dead.append(&mut self.chain);
            last_delta = 0;
            self.force_snapshot = false;
        }
        for chunk in delta.chunks(PER_DELTA) {
            let mut b = [0u8; BS];
            b[0..4].copy_from_slice(&DELTA_MAGIC.to_le_bytes());
            b[4..12].copy_from_slice(&last_delta.to_le_bytes());
            b[12..16].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            for (i, (k, p)) in chunk.iter().enumerate() {
                b[16 + i * 16..24 + i * 16].copy_from_slice(&k.to_le_bytes());
                b[24 + i * 16..32 + i * 16].copy_from_slice(&p.to_le_bytes());
            }
            seal(&mut b);
            let pba = self.alloc()?;
            self.dev_write(pba, &b)?;
            last_delta = pba;
            self.chain.push(pba);
        }
        self.write_checkpoint(last_delta)?;
        // Only now are the superseded blocks really dead: the previous checkpoint
        // stays valid until this one lands, so they were untouchable until here.
        for p in dead {
            let s = self.seg_of(p);
            self.live[s] -= 1;
        }
        Ok(())
    }

    fn write_checkpoint(&mut self, last_delta: u64) -> Result<(), Error> {
        self.seq += 1;
        let mut b = [0u8; BS];
        b[0..4].copy_from_slice(&CP_MAGIC.to_le_bytes());
        b[4..12].copy_from_slice(&self.seq.to_le_bytes());
        let head = if self.cur_seg.is_some() { self.head } else { 0 };
        b[12..20].copy_from_slice(&head.to_le_bytes());
        b[20..28].copy_from_slice(&last_delta.to_le_bytes());
        b[28..32].copy_from_slice(&(self.chain.len() as u32).to_le_bytes());
        seal(&mut b);
        self.dev_write(self.seq % 2, &b)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::rc::Rc;
    use alloc::vec;
    use core::cell::{Cell, RefCell};

    /// RAM disk that silently drops writes once `budget` hits zero (power loss).
    #[derive(Clone)]
    struct Disk {
        data: Rc<RefCell<Vec<[u8; BS]>>>,
        budget: Rc<Cell<i64>>,
        log: Rc<RefCell<Vec<u64>>>,
    }

    impl Disk {
        fn new(blocks: usize) -> Self {
            Disk {
                data: Rc::new(RefCell::new(vec![[0u8; BS]; blocks])),
                budget: Rc::new(Cell::new(i64::MAX)),
                log: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl BlockDev for Disk {
        fn blocks(&self) -> u64 {
            self.data.borrow().len() as u64
        }
        fn read(&self, b: u64, out: &mut [u8; BS]) -> Result<(), Error> {
            *out = self.data.borrow().get(b as usize).copied().ok_or(Error::Io)?;
            Ok(())
        }
        fn write(&mut self, b: u64, buf: &[u8; BS]) -> Result<(), Error> {
            if self.budget.get() <= 0 {
                return Ok(());
            }
            self.budget.set(self.budget.get() - 1);
            self.log.borrow_mut().push(b);
            self.data.borrow_mut()[b as usize] = *buf;
            Ok(())
        }
    }

    fn blk(v: u8) -> [u8; BS] {
        [v; BS]
    }

    #[test]
    fn basic_persist_and_remount() {
        let disk = Disk::new(256);
        let mut fs = Lfs::format(disk.clone()).unwrap();
        fs.write_block(10, &blk(1));
        fs.write_block(20, &blk(2));
        fs.sync().unwrap();
        fs.write_block(10, &blk(3));
        fs.delete_block(20);
        fs.sync().unwrap();
        let fs = Lfs::mount(disk).unwrap();
        let mut o = [0u8; BS];
        fs.read_block(10, &mut o).unwrap();
        assert_eq!(o, blk(3));
        assert_eq!(fs.read_block(20, &mut o), Err(Error::NotFound));
        assert_eq!(fs.len(), 1);
    }

    #[test]
    fn writes_are_sequential_and_coalesced() {
        let disk = Disk::new(1024);
        let mut fs = Lfs::format(disk.clone()).unwrap();
        disk.log.borrow_mut().clear();
        // 1000 random-order overwrites of 8 hot blocks
        let mut x = 7u32;
        for i in 0..1000u32 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            fs.write_block(((x >> 16) % 8) as u64 * 1000, &blk(i as u8));
        }
        fs.sync().unwrap();
        assert_eq!(fs.stats.logical_writes, 1000);
        assert!(fs.stats.device_writes <= 8 + 1 + 1 + 1, "{:?}", fs.stats);
        // data + delta writes strictly ascending (sequential), checkpoint is last
        let log = disk.log.borrow();
        let body = &log[..log.len() - 1];
        assert!(body.windows(2).all(|w| w[1] == w[0] + 1), "{:?}", log);
    }

    #[test]
    fn long_chains_get_snapshotted() {
        let disk = Disk::new(4096);
        let mut fs = Lfs::format(disk.clone()).unwrap();
        for i in 0..100u64 {
            fs.write_block(i % 5, &blk(i as u8));
            fs.sync().unwrap();
        }
        assert!(fs.chain.len() <= MAX_CHAIN + 1);
        let fs = Lfs::mount(disk).unwrap();
        let mut o = [0u8; BS];
        fs.read_block(4, &mut o).unwrap();
        assert_eq!(o, blk(99));
    }

    #[test]
    fn crash_at_every_write_point_is_atomic() {
        // Establish state A, then crash during the sync that produces state B.
        for cut in 0..12 {
            let disk = Disk::new(256);
            let mut fs = Lfs::format(disk.clone()).unwrap();
            fs.write_block(1, &blk(0xA1));
            fs.write_block(2, &blk(0xA2));
            fs.sync().unwrap();
            disk.budget.set(cut);
            fs.write_block(1, &blk(0xB1));
            fs.write_block(3, &blk(0xB3));
            fs.delete_block(2);
            let _ = fs.sync();
            let m = Lfs::mount(disk.clone()).unwrap();
            let mut o = [0u8; BS];
            let a = m.read_block(1, &mut o).is_ok() && o == blk(0xA1);
            if a {
                assert!(m.read_block(2, &mut o).is_ok() && o == blk(0xA2), "cut {cut}: torn A");
                assert!(m.read_block(3, &mut o).is_err(), "cut {cut}: torn A");
            } else {
                m.read_block(1, &mut o).unwrap();
                assert_eq!(o, blk(0xB1), "cut {cut}");
                assert!(m.read_block(2, &mut o).is_err(), "cut {cut}: torn B");
                m.read_block(3, &mut o).unwrap();
                assert_eq!(o, blk(0xB3), "cut {cut}");
            }
        }
    }

    #[test]
    fn full_device_reports_nospace() {
        let disk = Disk::new(8);
        let mut fs = Lfs::format(disk).unwrap();
        for k in 0..10 {
            fs.write_block(k, &blk(1));
        }
        assert_eq!(fs.sync(), Err(Error::NoSpace));
    }

    // ---- garbage collection ----

    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            self.0 >> 8
        }
    }

    type Model = alloc::collections::BTreeMap<u64, u8>;

    /// Rewrite `per_round` random keys (out of `keys`) each round, syncing every round.
    fn churn(fs: &mut Lfs<Disk>, model: &mut Model, rng: &mut Lcg, rounds: usize, keys: u32, per_round: usize) -> Result<(), Error> {
        for r in 0..rounds {
            for _ in 0..per_round {
                let k = (rng.next() % keys) as u64;
                let v = (rng.next() & 0xFF) as u8 | 1;
                fs.write_block(k, &blk(v));
                model.insert(k, v);
            }
            if r % 7 == 3 {
                let k = (rng.next() % keys) as u64;
                fs.delete_block(k);
                model.remove(&k);
            }
            fs.sync()?;
        }
        Ok(())
    }

    /// Write-once "cold" keys (1000..) interleaved with hot ones, one sync each, so every
    /// segment ends up holding a cold block that pins it and garbage piles up around it.
    fn prefill(fs: &mut Lfs<Disk>, model: &mut Model, cold: u64) {
        for i in 0..cold {
            fs.write_block(1000 + i, &blk(0x80 | i as u8));
            model.insert(1000 + i, 0x80 | i as u8);
            fs.write_block(i % 10, &blk(i as u8 | 1));
            model.insert(i % 10, i as u8 | 1);
            fs.sync().unwrap();
        }
    }

    fn check(fs: &Lfs<Disk>, model: &Model, keys: u32) {
        let mut o = [0u8; BS];
        for k in 0..keys as u64 {
            match model.get(&k) {
                Some(&v) => {
                    fs.read_block(k, &mut o).unwrap_or_else(|e| panic!("key {k}: {e:?}"));
                    assert_eq!(o, blk(v), "key {k}");
                }
                None => assert!(fs.read_block(k, &mut o).is_err(), "key {k} should be absent"),
            }
        }
    }

    #[test]
    fn churn_far_beyond_capacity_is_survivable_with_gc() {
        let disk = Disk::new(256); // 31 segments of 8 blocks
        let mut fs = Lfs::format(disk.clone()).unwrap();
        let (mut model, mut rng) = (Model::new(), Lcg(42));
        prefill(&mut fs, &mut model, 40);
        // ~1500 more block writes onto a 256-block device that is partly pinned by cold data
        churn(&mut fs, &mut model, &mut rng, 250, 20, 5).unwrap();
        assert!(fs.stats.gc_runs > 0, "cleaner never ran: {:?}", fs.stats);
        check(&fs, &model, 20);
        let fs2 = Lfs::mount(disk).unwrap();
        check(&fs2, &model, 20);
        for i in 0..40u64 {
            let mut o = [0u8; BS];
            fs2.read_block(1000 + i, &mut o).unwrap();
            assert_eq!(o, blk(0x80 | i as u8), "cold key {i}");
        }
        assert_eq!(fs2.len(), model.len());
    }

    #[test]
    fn delete_returns_space() {
        let disk = Disk::new(512);
        let mut fs = Lfs::format(disk).unwrap();
        for k in 0..100 {
            fs.write_block(k, &blk(k as u8 | 1));
        }
        fs.sync().unwrap();
        let used = fs.free_blocks();
        for k in 0..100 {
            fs.delete_block(k);
        }
        fs.sync().unwrap();
        while fs.gc().unwrap() {}
        assert!(fs.free_blocks() > used + 80, "{} -> {}", used, fs.free_blocks());
        assert_eq!(fs.len(), 0);
    }

    #[test]
    fn crash_during_cleaning_is_atomic() {
        // History: churn until the cleaner is active, then one more batch whose
        // sync must clean first. Power-cut at every write point of that sync.
        let mut cuts_that_cleaned = 0;
        for cut in 0..90 {
            let disk = Disk::new(256);
            let mut fs = Lfs::format(disk.clone()).unwrap();
            let (mut before, mut rng) = (Model::new(), Lcg(7));
            prefill(&mut fs, &mut before, 30);
            churn(&mut fs, &mut before, &mut rng, 120, 18, 4).unwrap();
            let runs0 = fs.stats.gc_runs;
            assert!(runs0 > 0);
            let mut after = before.clone();
            disk.budget.set(cut);
            for k in 0..6u64 {
                let v = 0xC0 + k as u8;
                fs.write_block(k * 3, &blk(v));
                after.insert(k * 3, v);
            }
            let _ = fs.sync();
            if fs.stats.gc_runs > runs0 {
                cuts_that_cleaned += 1;
            }
            let m = Lfs::mount(disk.clone()).unwrap_or_else(|e| panic!("cut {cut}: mount {e:?}"));
            let mut o = [0u8; BS];
            let all_keys = || (0..18u64).chain(1000..1030);
            let is_before = all_keys().all(|k| match before.get(&k) {
                Some(&v) => m.read_block(k, &mut o).is_ok() && o == blk(v),
                None => m.read_block(k, &mut o).is_err(),
            });
            let is_after = all_keys().all(|k| match after.get(&k) {
                Some(&v) => m.read_block(k, &mut o).is_ok() && o == blk(v),
                None => m.read_block(k, &mut o).is_err(),
            });
            assert!(is_before || is_after, "cut {cut}: state is neither the old nor the new version");
        }
        assert!(cuts_that_cleaned > 0, "test never exercised a cleaning sync");
    }

    #[test]
    fn truly_full_reports_nospace_and_keeps_data() {
        let disk = Disk::new(128);
        let mut fs = Lfs::format(disk.clone()).unwrap();
        let mut stored = 0u64;
        let mut hit_full = false;
        for k in 0..400u64 {
            fs.write_block(k, &blk(k as u8 | 1));
            if fs.sync().is_err() {
                hit_full = true;
                break;
            }
            stored = k + 1;
        }
        assert!(hit_full && stored > 40, "stored {stored}");
        let m = Lfs::mount(disk).unwrap();
        let mut o = [0u8; BS];
        for k in 0..stored {
            m.read_block(k, &mut o).unwrap();
            assert_eq!(o, blk(k as u8 | 1));
        }
    }

    #[test]
    fn cleaner_moves_cold_data_and_keeps_it_intact() {
        let disk = Disk::new(256);
        let mut fs = Lfs::format(disk.clone()).unwrap();
        let (mut model, mut rng) = (Model::new(), Lcg(5));
        prefill(&mut fs, &mut model, 40);
        churn(&mut fs, &mut model, &mut rng, 30, 10, 3).unwrap();
        let free_before = fs.free_blocks();
        let mut passes = 0;
        while fs.gc().unwrap() && passes < 100 {
            passes += 1;
        }
        assert!(fs.stats.gc_moved > 0, "{:?}", fs.stats);
        assert!(fs.free_blocks() >= free_before, "cleaning must not lose space");
        let fs2 = Lfs::mount(disk).unwrap();
        for i in 0..40u64 {
            let mut o = [0u8; BS];
            fs2.read_block(1000 + i, &mut o).unwrap();
            assert_eq!(o, blk(0x80 | i as u8));
        }
        check(&fs2, &model, 10);
    }
}
