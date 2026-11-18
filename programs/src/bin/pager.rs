#![no_std]
#![no_main]
use core::sync::atomic::{AtomicU32, Ordering};
use metal_programs::fs::{self, OpenOptions, SeekFrom};
use metal_programs::io::{Read, Write};
use metal_programs::thread::wait_word;
use metal_programs::*;

// The pager (kernel/src/pager.rs): writes pages the kernel pushes out of its compressed store to a swap file, and reads
// them back when a program touches them. `pager <slots>`: the argument is the size of /data/swap in pages. It needs file
// access and a MemCtl capability with the pager right in slot 1.
//
// Two misbehaving modes for the kernel's tests (the argument's bits 16 and up): 1 = snoop: look in every page it is asked to
// write for the plaintext of the test victim (`pagetest`) and exit, after 48 writes or a while, with leaks | writes << 8;
// 2 = tamper: change a byte of every page it hands back.
const MEMCTL: u64 = 18;
const MC_REGISTER: u64 = 6;
const MC_NEXT: u64 = 7;
const MC_DONE: u64 = 8;
const MEM_SLOT: u64 = 1;
const PAGE: usize = 4096;
const FILE: &str = "/data/swap";

#[repr(align(4096))]
struct Buf([u8; PAGE]);
static mut BUF: Buf = Buf([0; PAGE]);
/// The kernel changes this word and wakes us when there is a job.
static WORD: AtomicU32 = AtomicU32::new(0);

fn buf() -> *mut u8 {
    // SAFETY: `addr_of_mut!` makes a raw pointer to the static without creating a reference; this does not read or write it.
    unsafe { core::ptr::addr_of_mut!(BUF) as *mut u8 }
}

fn prepare(slots: u64) -> bool {
    let want = slots * PAGE as u64;
    if fs::metadata(FILE).map_or(false, |m| m.len >= want) {
        return true;
    }
    let Ok(mut f) = fs::File::create(FILE) else { return false };
    let zero = [0u8; PAGE];
    for _ in 0..slots {
        if f.write_all(&zero).is_err() {
            return false;
        }
    }
    f.close().is_ok()
}

fn io_slot(slot: u64, write: bool) -> bool {
    let mut o = OpenOptions::new();
    o.read(!write).write(write);
    let Ok(mut f) = o.open(FILE) else { return false };
    if f.seek(SeekFrom::Start(slot * PAGE as u64)).is_err() {
        return false;
    }
    // SAFETY: `buf()` is the page-sized static BUF; the pager is single-threaded and the kernel touches the buffer only inside the MC_NEXT/MC_DONE calls (docs/ABI.md), never while this slice is in use.
    let data = unsafe { core::slice::from_raw_parts_mut(buf(), PAGE) };
    let ok = if write {
        f.write_all(data).is_ok()
    } else {
        let mut got = 0;
        while got < PAGE {
            match f.read(&mut data[got..]) {
                Ok(n) if n > 0 => got += n,
                _ => break,
            }
        }
        got == PAGE
    };
    // closing is what commits a written file: the page counts as on disk only if that worked
    ok && f.close().is_ok()
}

/// The plaintext of word `w` of page `p` in the victim's first generation (programs/src/bin/pagetest.rs).
fn victim_word(p: usize, w: usize) -> u64 {
    ((p as u64) << 40 | w as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5851_F42D
}

/// Does the page in the buffer start like one of the victim's?
fn looks_like_victim() -> bool {
    // SAFETY: `buf()` is the page-sized static BUF, single-threaded use.
    let first = unsafe { core::ptr::read_volatile(buf() as *const u64) };
    (0..48).any(|p| victim_word(p, 0) == first)
}

program!("pager", |c| -> u64 {
    let mode = c.arg >> 16;
    let slots = (c.arg & 0xFFFF).max(1);
    let (mut leaks, mut writes) = (0u64, 0u64);
    let started = ticks();
    if buf() as usize % PAGE != 0 {
        println!("[pager] buffer not page aligned");
        return 1;
    }
    if !prepare(slots) {
        println!("[pager] cannot create {FILE}");
        return 2;
    }
    let r = svc(MEMCTL, MEM_SLOT, MC_REGISTER, slots, &WORD as *const AtomicU32 as u64, 0)[0] as i64;
    if r != 0 {
        println!("[pager] cannot register ({r})");
        return 3;
    }
    println!("[pager] serving {slots} pages from {FILE}");
    loop {
        let seen = WORD.load(Ordering::SeqCst);
        let job = svc(MEMCTL, MEM_SLOT, MC_NEXT, buf() as u64, 0, 0);
        match job[0] as i64 {
            0 => {
                if mode == 1 && ticks() > started + 800 {
                    return leaks | writes << 8;
                }
                wait_word(&WORD, seen, 50);
            }
            1 => {
                if mode == 1 {
                    writes += 1;
                    leaks += looks_like_victim() as u64;
                }
                let ok = io_slot(job[1], true);
                svc(MEMCTL, MEM_SLOT, MC_DONE, !ok as u64, 0, 0);
                if mode == 1 && writes >= 48 {
                    return leaks | writes << 8;
                }
            }
            2 => {
                let ok = io_slot(job[1], false);
                if mode == 2 {
                    // SAFETY: `buf()` is the page-sized static BUF, single-threaded use.
                    unsafe { *buf() ^= 1 };
                }
                svc(MEMCTL, MEM_SLOT, MC_DONE, !ok as u64, buf() as u64, 0);
            }
            e => {
                println!("[pager] job error {e}");
                sleep(10);
            }
        }
    }
});
