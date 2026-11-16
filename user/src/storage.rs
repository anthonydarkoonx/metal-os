//! Storage servers (each runs as its own restartable task) and the client-side
//! tests for them.
//!
//!   client --IPC--> rofs-server --IPC--> blk-driver --virtio--> disk
//!   client --IPC--> lfs-server  --IPC--> blk-driver
//!
//! Bulk data moves through shared pages (no copies through the kernel); the
//! block driver picks each caller's DMA buffer from the kernel-provided sender id.
use crate::*;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

// disk layout (must match tools/mkdisk.py)
const ROFS_BASE: u64 = 1024;
const ROFS_SECTORS: u64 = 12288;
const LFS_BASE: u64 = 13312;
const LFS_SECTORS: u64 = 11264; // (what the data store gets when the block driver cannot say how big the disk is: the small test disk)

// protocols
const ROFS_READ: u64 = 30; // path in shm[0..256) -> data in shm[256..)
const ROFS_STAT: u64 = 32; // -> reply [0, size, kind(1 file / 2 dir)]
const ROFS_LIST: u64 = 31; // path in shm[0..256) -> '\n'-separated names in shm[256..)
const LFS_PUT: u64 = 40; // w1 = key, 4 KiB block in shm
const LFS_GET: u64 = 41;
const LFS_SYNC: u64 = 42; // reply: device_writes, logical_writes
const LFS_CRASH: u64 = 43;
const LFS_DEL: u64 = 44;
const LFS_STAT: u64 = 45; // reply: gc_runs, gc_moved, free_blocks
const LFS_FAIL_GET: u64 = 46; // test hook: the next LFS_GET reports an I/O error
/// Reply: this instance's epoch. Every store instance has its own; a restarted one has a larger one, and has lost
/// every block that was staged but not yet synced. `LFS_SYNC` takes an expected epoch in w[2] and refuses (status 3,
/// nothing synced) if this is not that instance: a client that staged blocks with a predecessor must not be told
/// they are durable because the successor had nothing to sync.
const LFS_EPOCH: u64 = 47;
const DATA_OFF: usize = 256;

const SERVER_BUF_VA: usize = 0x8022_0000;
const SERVER_SHM_VA: usize = 0x8023_0000;

// ---------------------------------------------------------------------------
// block device access over IPC
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct BlkDev {
    ep: u64,
    buf: usize,
    base: u64,
    blocks: u64,
}

impl BlkDev {
    /// Retries when the driver dies mid-request: it is restarted by the kernel
    /// and block I/O is idempotent, so replaying is safe.
    fn xfer(&self, op: u64, blk: u64) -> Result<(), ()> {
        if blk >= self.blocks {
            return Err(());
        }
        for _ in 0..8 {
            let (st, r) = call(self.ep, [op, self.base + blk * 8, 8]);
            if st == E_DEAD {
                sleep(2);
                continue;
            }
            return if st == 0 && r[0] == 0 { Ok(()) } else { Err(()) };
        }
        Err(())
    }
    fn read_block(&self, blk: u64, out: &mut [u8; 4096]) -> Result<(), ()> {
        self.xfer(BLK_READ, blk)?;
        unsafe { core::ptr::copy_nonoverlapping(self.buf as *const u8, out.as_mut_ptr(), 4096) };
        Ok(())
    }
    fn write_block(&self, blk: u64, data: &[u8; 4096]) -> Result<(), ()> {
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.buf as *mut u8, 4096) };
        self.xfer(BLK_WRITE, blk)
    }
}

impl metal_lfs::BlockDev for BlkDev {
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn read(&self, blk: u64, buf: &mut [u8; 4096]) -> Result<(), metal_lfs::Error> {
        self.read_block(blk, buf).map_err(|_| metal_lfs::Error::Io)
    }
    fn write(&mut self, blk: u64, buf: &[u8; 4096]) -> Result<(), metal_lfs::Error> {
        self.write_block(blk, buf).map_err(|_| metal_lfs::Error::Io)
    }
}

/// EROFS byte source with a one-block cache.
struct BlkSource {
    dev: BlkDev,
    cache: RefCell<(u64, Box<[u8; 4096]>)>,
}

impl metal_erofs::Source for BlkSource {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), metal_erofs::Error> {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let (idx, inb) = (pos / 4096, (pos % 4096) as usize);
            let mut c = self.cache.borrow_mut();
            if c.0 != idx {
                self.dev.read_block(idx, &mut c.1).map_err(|_| metal_erofs::Error::Io)?;
                c.0 = idx;
            }
            let n = (buf.len() - done).min(4096 - inb);
            buf[done..done + n].copy_from_slice(&c.1[inb..inb + n]);
            done += n;
        }
        Ok(())
    }
}

/// Each client of a storage server has its own shared page, chosen by the
/// kernel-provided sender id: 0 = test conductor, 7 = VFS server.
fn shm_for(sender: u64) -> Option<&'static mut [u8; 4096]> {
    let i = match sender {
        0 => 0,
        7 => 1,
        _ => return None,
    };
    Some(unsafe { &mut *((va(SERVER_SHM_VA + i * 0x1000)) as *mut [u8; 4096]) })
}

fn map_server_memory(buf_slot: u64) {
    if mem_map(buf_slot, SERVER_BUF_VA, true) != 0
        || mem_map(3, SERVER_SHM_VA, true) != 0
        || mem_map(4, SERVER_SHM_VA + 0x1000, true) != 0
    {
        println!("[storage] mapping failed");
        exit(1);
    }
}

// ---------------------------------------------------------------------------
// role 5: EROFS system-partition server (read-only)
// ---------------------------------------------------------------------------
pub fn rofs_server() -> ! {
    map_server_memory(2);
    let dev = BlkDev { ep: 1, buf: va(SERVER_BUF_VA), base: ROFS_BASE, blocks: ROFS_SECTORS / 8 };
    let src = BlkSource { dev, cache: RefCell::new((u64::MAX, Box::new([0; 4096]))) };
    let fs = match metal_erofs::Erofs::new(src) {
        Ok(fs) => fs,
        Err(e) => {
            println!("[rofs] mount failed: {e:?}");
            exit(1)
        }
    };
    println!("[rofs] EROFS system partition mounted (read-only)");
    loop {
        let m = recv(0);
        let Some(page) = shm_for(m.sender) else {
            reply([9, 0, 0]);
            continue;
        };
        let plen = page[..DATA_OFF].iter().position(|&c| c == 0).unwrap_or(DATA_OFF);
        let path = core::str::from_utf8(&page[..plen]).unwrap_or("");
        let ino = match fs.lookup(path) {
            Ok(i) => i,
            Err(_) => {
                reply([1, 0, 0]);
                continue;
            }
        };
        match m.w[0] {
            ROFS_STAT => {
                reply([0, ino.size, if ino.is_dir() { 2 } else { 1 }]);
            }
            ROFS_READ if ino.is_file() => {
                let len = (m.w[2] as usize).min(4096 - DATA_OFF);
                match fs.read(&ino, m.w[1], &mut page[DATA_OFF..DATA_OFF + len]) {
                    Ok(n) => reply([0, n as u64, ino.size]),
                    Err(_) => reply([2, 0, 0]),
                };
            }
            ROFS_LIST if ino.is_dir() => {
                let mut out = Vec::new();
                let _ = fs.read_dir(&ino, |n, _, ftype| {
                    if n != b"." && n != b".." {
                        out.extend_from_slice(n);
                        if ftype == 2 {
                            out.push(b'/'); // directories carry a trailing slash
                        }
                        out.push(b'\n');
                    }
                    true
                });
                let n = out.len().min(4096 - DATA_OFF);
                page[DATA_OFF..DATA_OFF + n].copy_from_slice(&out[..n]);
                reply([0, n as u64, 0]);
            }
            _ => {
                reply([3, 0, 0]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// role 6: log-structured data store server
// ---------------------------------------------------------------------------
pub fn lfs_server() -> ! {
    map_server_memory(2);
    // the data partition is the rest of the disk, whatever its size (tools/mkdisk.py leaves it to us)
    let sectors = match call(1, [BLK_INFO, 0, 0]) {
        (0, r) if r[0] >= LFS_BASE + 4096 => r[0] - LFS_BASE,
        _ => LFS_SECTORS,
    };
    let dev = BlkDev { ep: 1, buf: va(SERVER_BUF_VA), base: LFS_BASE, blocks: sectors / 8 };
    println!("[lfs] data partition: {} KiB", sectors / 2);
    let mut fs = match metal_lfs::Lfs::mount(dev) {
        Ok(fs) => {
            println!("[lfs] mounted existing store: {} blocks live", fs.len());
            fs
        }
        Err(_) => match metal_lfs::Lfs::format(dev) {
            Ok(fs) => {
                println!("[lfs] no valid checkpoint: formatted fresh store");
                fs
            }
            Err(e) => {
                println!("[lfs] format failed: {e:?}");
                exit(1)
            }
        },
    };
    let mut tmp = Box::new([0u8; 4096]);
    let mut fail_get = false;
    let epoch = ticks() + 1;
    loop {
        let m = recv(0);
        let key = m.w[1];
        let Some(sh) = shm_for(m.sender) else {
            reply([9, 0, 0]);
            continue;
        };
        match m.w[0] {
            LFS_PUT => {
                tmp.copy_from_slice(&sh[..]);
                fs.write_block(key, &tmp);
                reply([0, epoch, 0]);
            }
            // status 0 = found, 1 = there is no such block (a hole), 2 = the store could not tell (I/O error):
            // a client must never take an error for a hole, or a failed read turns into zeros
            LFS_GET => match if core::mem::take(&mut fail_get) { Err(metal_lfs::Error::Io) } else { fs.read_block(key, &mut tmp) } {
                Ok(()) => {
                    sh.copy_from_slice(&tmp[..]);
                    reply([0, epoch, 0]);
                }
                Err(metal_lfs::Error::NotFound) => {
                    reply([1, epoch, 0]);
                }
                Err(_) => {
                    reply([2, epoch, 0]);
                }
            },
            LFS_EPOCH => {
                reply([0, epoch, 0]);
            }
            LFS_FAIL_GET => {
                fail_get = true;
                reply([0, 0, 0]);
            }
            LFS_STAT => {
                reply([fs.stats.gc_runs, fs.stats.gc_moved, fs.free_blocks()]);
            }
            LFS_DEL => {
                fs.delete_block(key);
                reply([0, epoch, 0]);
            }
            LFS_SYNC => {
                if m.w[2] != 0 && m.w[2] != epoch {
                    reply([3, 0, 0]); // the caller staged its blocks with an earlier instance: they are gone
                    continue;
                }
                let st = fs.sync().is_ok();
                reply([!st as u64, fs.stats.device_writes, fs.stats.logical_writes]);
            }
            LFS_CRASH => {
                println!("[lfs] simulated fault with unsynced data in RAM");
                unsafe { write_volatile(0x3000 as *mut u64, 1) };
            }
            _ => {
                reply([9, 0, 0]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// client-side tests
// ---------------------------------------------------------------------------
const C_ROFS_SHM: usize = 0x8032_0000;
const C_LFS_SHM: usize = 0x8033_0000;

fn cshm(base: usize) -> &'static mut [u8; 4096] {
    unsafe { &mut *(base as *mut [u8; 4096]) }
}

fn set_path(p: &str) {
    let s = cshm(C_ROFS_SHM);
    s[..p.len()].copy_from_slice(p.as_bytes());
    s[p.len()] = 0;
}

fn rofs_read(path: &str, off: u64, len: u64) -> (i64, u64, u64) {
    set_path(path);
    let (st, r) = call(5, [ROFS_READ, off, len]);
    (if st != 0 { st } else { r[0] as i64 }, r[1], r[2])
}

fn block_pattern(key: u64, out: &mut [u8; 4096]) {
    for (i, b) in out.iter_mut().enumerate() {
        *b = (key * 17 + i as u64) as u8;
    }
}

fn lfs_put(key: u64) -> bool {
    block_pattern(key, cshm(C_LFS_SHM));
    let (st, r) = call(7, [LFS_PUT, key, 0]);
    st == 0 && r[0] == 0
}

fn lfs_get_ok(key: u64) -> bool {
    cshm(C_LFS_SHM).fill(0);
    let (st, r) = call(7, [LFS_GET, key, 0]);
    if st != 0 || r[0] != 0 {
        return false;
    }
    let mut want = [0u8; 4096];
    block_pattern(key, &mut want);
    cshm(C_LFS_SHM)[..] == want[..]
}

pub fn storage_tests() {
    if mem_map(6, C_ROFS_SHM, true) != 0 || mem_map(8, C_LFS_SHM, true) != 0 {
        println!("[client] TEST storage: cannot map server pages: FAIL");
        return;
    }

    // --- EROFS: read-only system image ---
    let (st, n, size) = rofs_read("/hello.txt", 0, 100);
    let want = b"Hello from the Metal OS system image!\n";
    let ok = st == 0 && n as usize == want.len() && size == n && &cshm(C_ROFS_SHM)[DATA_OFF..DATA_OFF + want.len()] == want;
    println!("[client] TEST EROFS read /hello.txt ({n} bytes): {}", verdict(ok));

    let mut all_ok = true;
    let mut off = 0u64;
    while off < 9000 {
        let (st, n, size) = rofs_read("/bin/blob.bin", off, 3840);
        let page = cshm(C_ROFS_SHM);
        all_ok &= st == 0 && size == 9000 && n > 0;
        for i in 0..n as usize {
            all_ok &= page[DATA_OFF + i] == (((off as usize + i) * 7) % 251) as u8;
        }
        off += n.max(1);
    }
    println!("[client] TEST EROFS read 9000-byte file across blocks: {}", verdict(all_ok));

    set_path("/");
    let (st, r) = call(5, [ROFS_LIST, 0, 0]);
    let n = r[1] as usize;
    let names = String::from_utf8_lossy(&cshm(C_ROFS_SHM)[DATA_OFF..DATA_OFF + n]).into_owned();
    println!("[client] TEST EROFS list / ({:?}): {}", names.replace('\n', " ").trim_end(),
        verdict(st == 0 && names == "bin/\netc/\nhello.txt\nusr/\n"));
    let (st, _, _) = rofs_read("/no/such/file", 0, 10);
    println!("[client] TEST EROFS missing path rejected (status={st}): {}", verdict(st != 0));

    // --- LFS: random-ish overwrites coalesce into sequential log writes ---
    // The store is shared with the VFS and program, so measure this batch as a delta of the
    // server's cumulative counters.
    let (_, base) = call(7, [LFS_SYNC, 0, 0]);
    let mut ok = true;
    for k in 1..=5 {
        ok &= lfs_put(k);
    }
    for _ in 0..50 {
        ok &= lfs_put(3); // hot block rewritten 50 times before the sync
    }
    let (st, r) = call(7, [LFS_SYNC, 0, 0]);
    let (dev_w, log_w) = (r[1] - base[1], r[2] - base[2]);
    println!("[client] TEST LFS write coalescing ({log_w} logical writes -> {dev_w} device blocks): {}",
        verdict(ok && st == 0 && r[0] == 0 && log_w >= 55 && dev_w * 2 < log_w));

    // --- LFS crash consistency: synced data survives a server crash, unsynced does not ---
    let ok = lfs_put(99); // never synced
    let (st, _) = call(7, [LFS_CRASH, 0, 0]);
    println!("[client] TEST LFS server crash surfaces as error (status={st}, staged={ok}): {}", verdict(ok && st == E_DEAD));
    let synced_ok = (1..=5).all(lfs_get_ok);
    let (st99, r99) = call(7, [LFS_GET, 99, 0]);
    println!("[client] TEST LFS synced blocks survive restart, unsynced block lost: {}",
        verdict(synced_ok && st99 == 0 && r99[0] != 0));

    // --- whole-stack fault: the block driver dies under the LFS server's feet ---
    let mut ok = lfs_put(7);
    let (st, _) = call(3, [BLK_CRASH, 0, 0]); // driver restarts; servers retry transparently
    ok &= st == E_DEAD;
    let (st, r) = call(7, [LFS_SYNC, 0, 0]);
    ok &= st == 0 && r[0] == 0;
    println!("[client] TEST LFS sync across a block-driver crash: {}", verdict(ok && lfs_get_ok(7) && (1..=5).all(lfs_get_ok)));
}
