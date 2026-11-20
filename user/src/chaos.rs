//! Chaos test: kill the system's services at random moments while a workload uses them, then
//! check that nothing was lost or corrupted and that everything came back.
//!
//! Enabled by `fw_cfg opt/metal/chaos=<seed>` (`make chaos`). The *monkey* (role 15) holds the only
//! `Admin` capability; it sleeps a random time and kills one of the block driver, the two
//! filesystem servers, the VFS, the display driver or the keyboard driver, which the kernel then
//! restarts. The *workload* (the conductor, role 0) keeps using every one of them.
//!
//! What a read of a `/data` file may return is exact. A write that returned success is committed
//! (close commits in one checkpoint). A write that failed (the VFS or LFS died under it) may or
//! may not have landed, but it is atomic: the file then holds the old version or that new one, never
//! a mixture. So each file tracks `ok` (last confirmed version) and `maybe` (failed attempts since);
//! anything else, including a missing file that was once confirmed, is a bug.
use crate::proto::gpu::GPU_FLUSH;
use crate::proto::input::KBD_POLL;
use crate::vfs::{Vfs, ENOENT, O_READ};
use crate::*;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const MONKEY_START: u64 = 80;
const KILL_ROLE: u64 = 22;
const VICTIMS: [(u64, &str); 8] =
    [(4, "blk-driver"), (5, "rofs-server"), (6, "lfs-server"), (7, "vfs-server"), (12, "gpu-driver"), (14, "kbd-driver"), (18, "net-driver"), (19, "netd")];

const RUN_TICKS: u64 = 3000; // 30 s of chaos
const FILES: usize = 8;
const VFS_SHM: usize = 0x8034_0000;
const LEAK_SLACK_PAGES: u64 = 400;

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

// ---------------------------------------------------------------------------
// role 15: the monkey
// ---------------------------------------------------------------------------
const MEMCTL: u64 = 18;
const MC_SWAP_OUT: u64 = 4;
const MC_SWAP_STATS: u64 = 5;
const MCS_FORCE: u64 = 1;

pub fn monkey() -> ! {
    println!("[monkey] waiting for the workload to start it");
    let m = recv(1); // slot 0 is the Admin capability, slot 1 the endpoint the conductor starts us on
    let (duration, mut rng) = (m.w[1], m.w[2] | 1);
    reply([0, 0, 0]);
    let start = ticks();
    let mut kills = [0u64; VICTIMS.len()];
    let mut last = [0u64; VICTIMS.len()];
    let mut swapped = 0u64;
    while ticks() - start < duration {
        sleep(20 + xorshift(&mut rng) % 180); // between 0.2 and 2 seconds
        if xorshift(&mut rng) % 2 == 0 {
            // push random pages of any task out to the swap store: they fault back in whenever they are touched,
            // including while the task is being killed and restarted
            let budget = 8 + xorshift(&mut rng) % 200;
            swapped += svc(MEMCTL, 2, MC_SWAP_OUT, budget, MCS_FORCE, 0)[0];
        }
        let i = (xorshift(&mut rng) % VICTIMS.len() as u64) as usize;
        // Space kills of one service further apart than the kernel's restart-streak window (3 s): this
        // test is about recovery. Killing the same service in a tight loop is *meant* to make the kernel give up.
        if kills[i] > 0 && ticks() - last[i] < 450 {
            continue;
        }
        last[i] = ticks();
        let n = svc(KILL_ROLE, 0, VICTIMS[i].0, 0, 0, 0)[0] as i64;
        if n > 0 {
            kills[i] += 1;
            println!("[monkey] killed {}", VICTIMS[i].1);
        }
    }
    let list: Vec<String> = VICTIMS.iter().zip(kills).map(|((_, name), k)| format!("{name} {k}")).collect();
    let sw = svc(MEMCTL, 2, MC_SWAP_STATS, 0, 0, 0);
    println!("[monkey] done: {} kills ({}); {swapped} pages pushed to swap, {} brought back, {} still stored", kills.iter().sum::<u64>(), list.join(", "), sw[3], sw[0]);
    exit(0)
}

// ---------------------------------------------------------------------------
// role 0 in chaos mode: the workload
// ---------------------------------------------------------------------------
/// The bytes of version `round` of file `file`: length and contents both depend on both, so a mixture
/// of two versions can never equal either.
fn content(file: usize, round: u32) -> Vec<u8> {
    let len = 1000 + (file * 7919 + round as usize * 104729) % 19000;
    (0..len).map(|i| (i * 31 + file * 131 + round as usize * 17 + (i >> 8)) as u8).collect()
}

/// The first `n` bytes of /system/usr/share/lorem.txt, which the image builder generates.
fn lorem_prefix(n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while v.len() < n {
        v.extend_from_slice(format!("Metallium-OS system image, line {i:05}: the quick brown fox\n").as_bytes());
        i += 1;
    }
    v.truncate(n);
    v
}

fn path(f: usize) -> String {
    format!("/data/chaos{f}")
}

#[derive(Default)]
struct File {
    ok: Option<u32>,
    maybe: Vec<u32>,
    round: u32,
}

#[derive(Default)]
struct Stats {
    writes_ok: u64,
    writes_err: u64,
    reads_ok: u64,
    reads_err: u64,
    corrupt: u64,
    rofs_ok: u64,
    rofs_err: u64,
    gpu_ok: u64,
    gpu_err: u64,
    kbd_ok: u64,
    kbd_err: u64,
    blk_ok: u64,
    blk_err: u64,
}

struct Work {
    v: Vfs,
    files: [File; FILES],
    s: Stats,
}

enum Check {
    Valid,
    Transient,
    Corrupt,
}

impl Work {
    fn write(&mut self, f: usize) {
        self.files[f].round += 1;
        let r = self.files[f].round;
        match self.v.write_file(&path(f), &content(f, r)) {
            Ok(()) => {
                self.files[f].ok = Some(r);
                self.files[f].maybe.clear();
                self.s.writes_ok += 1;
            }
            Err(_) => {
                self.files[f].maybe.push(r); // may have landed anyway
                self.s.writes_err += 1;
            }
        }
    }

    /// Read file `f` and decide whether what came back is one of the legal versions.
    fn check(&mut self, f: usize) -> Check {
        let file = &mut self.files[f];
        match self.v.read_file(&path(f)) {
            Ok(bytes) => {
                let hit = file.ok.into_iter().chain(file.maybe.iter().copied()).find(|&r| bytes == content(f, r));
                match hit {
                    Some(r) => {
                        if file.ok != Some(r) {
                            // a write we were told had failed did land: it is the confirmed version now,
                            // and attempts before it can no longer appear
                            file.ok = Some(r);
                            file.maybe.retain(|&m| m > r);
                        }
                        Check::Valid
                    }
                    None => {
                        println!("[chaos] CORRUPT chaos{f}: {} bytes match no version (confirmed {:?}, unconfirmed {:?})", bytes.len(), file.ok, file.maybe);
                        // where does it differ from the version it most resembles? (whole 4 KiB blocks point at the storage
                        // stack, scattered bytes at memory)
                        for r in file.ok.into_iter().chain(file.maybe.iter().copied()) {
                            let want = content(f, r);
                            let diff: Vec<usize> = (0..bytes.len().min(want.len())).filter(|&i| bytes[i] != want[i]).collect();
                            if bytes.len() == want.len() || diff.len() < bytes.len() / 2 {
                                let blocks: Vec<usize> = { let mut b: Vec<usize> = diff.iter().map(|i| i / 4096).collect(); b.dedup(); b };
                                println!("[chaos]   vs version {r} ({} bytes): {} bytes differ, first at {:?}, last at {:?}, in 4K blocks {:?}; got {:02x?} where {:02x?} was expected",
                                    want.len(), diff.len(), diff.first(), diff.last(), blocks,
                                    diff.first().map(|&i| &bytes[i..(i + 8).min(bytes.len())]), diff.first().map(|&i| &want[i..(i + 8).min(want.len())]));
                            }
                        }
                        Check::Corrupt
                    }
                }
            }
            Err(e) if e == ENOENT as i64 => {
                if file.ok.is_some() {
                    println!("[chaos] LOST chaos{f}: a confirmed write is gone (confirmed {:?})", file.ok);
                    Check::Corrupt
                } else {
                    Check::Valid // never confirmed, so absent is a legal state
                }
            }
            Err(_) => Check::Transient,
        }
    }

    fn read(&mut self, f: usize) {
        match self.check(f) {
            Check::Valid => self.s.reads_ok += 1,
            Check::Transient => self.s.reads_err += 1,
            Check::Corrupt => self.s.corrupt += 1,
        }
    }

    fn rofs(&mut self) -> bool {
        let hello = self.v.read_file("/system/hello.txt").map_or(false, |b| b == b"Hello from the Metallium-OS system image!\n");
        let lorem = self.v.open("/system/usr/share/lorem.txt", O_READ).ok().map_or(false, |fd| {
            let mut buf = alloc::vec![0u8; 4096];
            let n = self.v.read(fd, &mut buf);
            let _ = self.v.close(fd);
            n == Ok(4096) && buf == lorem_prefix(4096)
        });
        hello && lorem
    }
}

fn gpu_ok() -> bool {
    let (st, r) = call(21, [GPU_FLUSH, 64 << 32 | 64 << 48, 0]);
    st == 0 && r[0] == 0
}
fn kbd_ok() -> bool {
    call(14, [KBD_POLL, 0, 0]).0 == 0
}
fn blk_ok() -> bool {
    let (st, r) = call(3, [BLK_INFO, 0, 0]);
    st == 0 && r[0] > 0
}

/// Retry `f` for up to `ticks` until it holds.
fn eventually(ticks_allowed: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = ticks() + ticks_allowed;
    loop {
        if f() {
            return true;
        }
        if ticks() > end {
            return false;
        }
        sleep(5);
    }
}

pub fn run() -> ! {
    let seed = sysinfo(2)[2];
    println!("[chaos] online, seed {seed}");
    if mem_map(10, VFS_SHM, true) != 0 {
        println!("[chaos] TEST setup: cannot map the VFS page: FAIL");
        exit(1)
    }
    vfs::wait_for_program(); // the program task's own test must not be disturbed
    let mut w = Work { v: Vfs::new(9, VFS_SHM), files: Default::default(), s: Stats::default() };

    // warm-up without chaos: every file gets a confirmed version
    for f in 0..FILES {
        w.write(f);
    }
    let warm = w.files.iter().all(|f| f.ok.is_some());
    println!("[chaos] TEST warm-up: {FILES} files written and confirmed: {}", verdict(warm));
    let free0 = sysinfo(0)[3];
    let restarts0 = sysinfo(1)[4];

    // a network workload in a program of its own: it keeps echoing data through TCP while the network services die
    let net_prog = w.v.read_file("/system/bin/nettest").ok().filter(|i| i.len() >= 32).and_then(|image| {
        let mut rq = [0u8; 96];
        rq[4..8].copy_from_slice(&2u32.to_le_bytes()); // network access
        rq[28..32].copy_from_slice(&300u32.to_le_bytes()); // heap pages
        rq[32..40].copy_from_slice(&(14 | (RUN_TICKS / 100 + 5) << 8).to_le_bytes());
        rq[64..68].copy_from_slice(&[0xFF, 0, 0, 0]);
        for i in 0..8 {
            rq[64 + 4 * i] = 0xFF;
        }
        let t = metal_loader::spawn(|id, a1, a2, a3, a4| svc(id, a1, a2, a3, a4, 0)[0], 26, &mut rq, &image);
        (t > 0).then_some(t)
    });
    println!("[chaos] network workload: {}", if net_prog.is_some() { "started" } else { "not available (no network device)" });

    let (st, _) = call(22, [MONKEY_START, RUN_TICKS, seed]);
    println!("[chaos] monkey started (status {st}); {} s of chaos", RUN_TICKS / 100);
    let start = ticks();
    let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15 | 1;
    while ticks() - start < RUN_TICKS + 200 {
        let f = (xorshift(&mut rng) % FILES as u64) as usize;
        match xorshift(&mut rng) % 100 {
            0..=39 => w.write(f),
            40..=69 => w.read(f),
            70..=77 => {
                if w.rofs() { w.s.rofs_ok += 1 } else { w.s.rofs_err += 1 }
            }
            78..=85 => {
                if gpu_ok() { w.s.gpu_ok += 1 } else { w.s.gpu_err += 1 }
            }
            86..=91 => {
                if kbd_ok() { w.s.kbd_ok += 1 } else { w.s.kbd_err += 1 }
            }
            _ => {
                if blk_ok() { w.s.blk_ok += 1 } else { w.s.blk_err += 1 }
            }
        }
        sleep(1);
    }
    sleep(300); // let the last restarts settle
    let net_code = net_prog.map(|t| svc(25, t as u64, 0, 0, 0, 0)[0] as i64);
    if let Some(code) = net_code {
        println!("[chaos] TEST the network carried only correct data while its driver and stack were killed, and works again afterwards (program exit {code}): {}", verdict(code == 0));
    }

    let s = &w.s;
    println!(
        "[chaos] workload: writes {} ok / {} failed, reads {} ok / {} failed, rofs {}/{}, display {}/{}, keyboard {}/{}, block info {}/{} (ok/failed during the chaos)",
        s.writes_ok, s.writes_err, s.reads_ok, s.reads_err, s.rofs_ok, s.rofs_err, s.gpu_ok, s.gpu_err, s.kbd_ok, s.kbd_err, s.blk_ok, s.blk_err
    );
    let restarts = sysinfo(1)[4] - restarts0;
    println!("[chaos] TEST the monkey made the system restart services ({restarts} restarts): {}", verdict(restarts >= 10));
    println!("[chaos] TEST no corruption and no lost confirmed write in {} verified reads: {}", w.s.reads_ok, verdict(w.s.corrupt == 0 && w.s.reads_ok > 0));

    // after the chaos: every service answers again
    let (mut blk, mut gpu, mut kbd, mut rofs) = (false, false, false, false);
    blk |= eventually(500, blk_ok);
    gpu |= eventually(500, gpu_ok);
    kbd |= eventually(500, kbd_ok);
    rofs |= eventually(500, || w.rofs());
    let vfs_up = eventually(500, || w.v.list("/data").is_ok());
    println!("[chaos] TEST every service answers again afterwards (block {blk}, display {gpu}, keyboard {kbd}, rofs {rofs}, vfs {vfs_up}): {}", verdict(blk && gpu && kbd && rofs && vfs_up));

    // final pass: every file reads, and is a legal version
    let mut all = true;
    for f in 0..FILES {
        let mut state = Check::Transient;
        for _ in 0..100 {
            state = w.check(f);
            if !matches!(state, Check::Transient) {
                break;
            }
            sleep(5);
        }
        all &= matches!(state, Check::Valid);
    }
    println!("[chaos] TEST all {FILES} files read back as a version that was written: {}", verdict(all));

    // memory: killing and restarting must give back what it takes
    let free1 = sysinfo(0)[3];
    println!("[chaos] free frames {free0} -> {free1} across {restarts} restarts");
    println!("[chaos] TEST restarts do not leak memory (at most {LEAK_SLACK_PAGES} pages lost): {}", verdict(free1 + LEAK_SLACK_PAGES >= free0));
    println!("[chaos] done");
    exit(0)
}
