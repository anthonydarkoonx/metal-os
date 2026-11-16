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

program!("pager", |c| -> u64 {
    let slots = c.arg.max(1);
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
                wait_word(&WORD, seen, 50);
            }
            1 => {
                let ok = io_slot(job[1], true);
                svc(MEMCTL, MEM_SLOT, MC_DONE, !ok as u64, 0, 0);
            }
            2 => {
                let ok = io_slot(job[1], false);
                svc(MEMCTL, MEM_SLOT, MC_DONE, !ok as u64, buf() as u64, 0);
            }
            e => {
                println!("[pager] job error {e}");
                sleep(10);
            }
        }
    }
});
