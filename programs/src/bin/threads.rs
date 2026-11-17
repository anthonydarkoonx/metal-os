#![no_std]
#![no_main]
use core::sync::atomic::{AtomicU32, Ordering::*};
use metal_programs::fs::*;
use metal_programs::thread::{self, Mutex};
use metal_programs::*;

// Tests of threads. The argument selects the scenario; the exit code is 0 when it behaved (or says what did not).
// Capabilities the threads need: bit 8 = the VFS endpoint.

static LOCK: Mutex = Mutex::new();
static SHARED: AtomicU32 = AtomicU32::new(0);
static DONE: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
static TURN: AtomicU32 = AtomicU32::new(0);
static THEIRS: AtomicU32 = AtomicU32::new(0);
static GATE: AtomicU32 = AtomicU32::new(0);
static RELEASE: AtomicU32 = AtomicU32::new(0);
static RESULT: AtomicU32 = AtomicU32::new(0);
static PAGES: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static mut FS: Option<Fs> = None;
/// Capability slots the threads are given (bit 8: the VFS endpoint, once the file system is in use).
static CAPS: AtomicU32 = AtomicU32::new(0);

const STACK_PAGES: usize = 2;

/// A stack from the heap: its top.
fn stack() -> usize {
    let base = heap_grow(STACK_PAGES as u64);
    if base < 0 {
        exit(900 + (-base) as u64);
    }
    base as usize + STACK_PAGES * 4096
}

fn start(f: extern "C" fn(u64) -> !, arg: u64) -> i64 {
    thread::create(f, stack(), arg, CAPS.load(Relaxed))
}

fn count_under_lock(rounds: u32, hold: bool) {
    for _ in 0..rounds {
        LOCK.lock();
        let v = SHARED.load(Relaxed);
        if hold {
            yield_now(); // hold the lock across a reschedule: the others must sleep, not spin through it
        }
        SHARED.store(v + 1, Relaxed); // (not an atomic increment: only the lock keeps this right)
        LOCK.unlock();
    }
}

extern "C" fn counter(id: u64) -> ! {
    count_under_lock(1000, id == 1);
    thread::exit(&DONE[id as usize % 8])
}

extern "C" fn pong(_: u64) -> ! {
    for _ in 0..500 {
        while TURN.load(Acquire) != 1 {
            thread::wait_word(&TURN, 0, 0);
        }
        THEIRS.fetch_add(1, Relaxed);
        TURN.store(0, Release);
        thread::wake_word(&TURN, 1);
    }
    thread::exit(&DONE[0])
}

extern "C" fn waiter(_: u64) -> ! {
    RESULT.store(thread::wait_word(&GATE, 0, 1000) as u32, SeqCst);
    thread::exit(&DONE[1])
}

extern "C" fn crasher(_: u64) -> ! {
    unsafe { core::ptr::write_volatile(8 as *mut u32, 1) };
    loop {}
}

extern "C" fn sleeper(_: u64) -> ! {
    loop {
        thread::wait_word(&GATE, 0, 0);
    }
}

extern "C" fn exiter(code: u64) -> ! {
    exit(code)
}

extern "C" fn spinner(_: u64) -> ! {
    loop {
        SHARED.fetch_add(1, Relaxed);
    }
}

/// Waits until `RELEASE` is set, then ends (reporting through `DONE[2]`).
extern "C" fn releasable(_: u64) -> ! {
    while RELEASE.load(Acquire) == 0 {
        thread::wait_word(&RELEASE, 0, 0);
    }
    thread::exit(&DONE[2])
}

extern "C" fn quick(_: u64) -> ! {
    SHARED.fetch_add(1, Relaxed);
    thread::exit(&DONE[2])
}

/// Ends by naming a word in the program's own *code* as the word the kernel stores a 1 into and wakes: the kernel must refuse (the code
/// page is read-only for the program), not write through its own mapping.
extern "C" fn ro_exit(word: u64) -> ! {
    DONE[0].store(1, SeqCst);
    svc(thread::THREAD_EXIT, word, 1, 0, 0, 0);
    loop {}
}

extern "C" fn heaper(id: u64) -> ! {
    let p = heap_grow(1);
    if p > 0 {
        unsafe { core::ptr::write_volatile(p as *mut u64, 0xABCD_0000 + id) };
    }
    PAGES[id as usize % 4].store(p as u32, SeqCst);
    SHARED.fetch_add(if p > 0 { 1 } else { 100 }, SeqCst);
    thread::exit(&DONE[id as usize % 8])
}

extern "C" fn reader(_: u64) -> ! {
    LOCK.lock();
    let r = (|| -> u64 {
        let fs = unsafe { (*core::ptr::addr_of!(FS)).as_ref() }.unwrap();
        let fd = match fs.open_file("/system/hello.txt", O_READ) {
            Ok(fd) => fd,
            Err(e) => return 2000 + e as u64,
        };
        let mut buf = [0u8; 100];
        let n = match fs.read(fd, &mut buf) {
            Ok(n) => n,
            Err(e) => return 3000 + e as u64,
        };
        let _ = fs.close(fd);
        n as u64
    })();
    LOCK.unlock();
    RESULT.store(r as u32, SeqCst);
    thread::exit(&DONE[3])
}

program!("threads", |c| {
    match c.arg {
        // four threads and the main task increment a shared counter under the lock
        0 => {
            for i in 0..4 {
                if start(counter, i) < 0 {
                    return 1;
                }
            }
            count_under_lock(1000, false);
            for i in 0..4 {
                thread::join(&DONE[i]);
            }
            let total = SHARED.load(SeqCst);
            if total == 5000 { 0 } else { total as u64 + 1 }
        }
        // strict alternation between this task and a thread through one word
        1 => {
            if start(pong, 0) < 0 {
                return 1;
            }
            let mut mine = 0;
            for _ in 0..500 {
                while TURN.load(Acquire) != 0 {
                    thread::wait_word(&TURN, 1, 0);
                }
                mine += 1;
                TURN.store(1, Release);
                thread::wake_word(&TURN, 1);
            }
            thread::join(&DONE[0]);
            if mine == 500 && THEIRS.load(SeqCst) == 500 { 0 } else { 1 }
        }
        // the answers of WAIT_WORD and WAKE_WORD
        2 => {
            let w = AtomicU32::new(0);
            let differs = thread::wait_word(&w, 1, 0);
            let t0 = ticks();
            let late = thread::wait_word(&w, 0, 4);
            let waited = ticks() - t0;
            let nobody = thread::wake_word(&w, 5);
            let bad = svc(thread::WAIT_WORD, 2, 0, 0, 0, 0)[0] as i64; // unaligned
            let bad2 = svc(thread::WAIT_WORD, 0x7000_0000, 0, 0, 0, 0)[0] as i64; // not mapped
            // a waiter that is woken before its time runs out
            if start(waiter, 0) < 0 {
                return 1;
            }
            sleep(3);
            GATE.store(1, SeqCst);
            let woken = thread::wake_word(&GATE, 1);
            thread::join(&DONE[1]);
            let r = RESULT.load(SeqCst) as i32 as i64;
            let mut code = 0;
            for (i, ok) in [differs == thread::E_AGAIN, late == thread::E_TIMEDOUT && waited >= 3, nobody == 0, bad < 0 && bad2 < 0, woken == 1, r == 0].into_iter().enumerate() {
                if !ok {
                    code |= 1 << i;
                }
            }
            code
        }
        // a thread faults: the whole program ends, crashed
        3 => {
            if start(sleeper, 0) < 0 || start(crasher, 0) < 0 {
                return 1;
            }
            loop {
                sleep(5);
            }
        }
        // this task exits while three threads sleep for ever
        4 => {
            for _ in 0..3 {
                if start(sleeper, 0) < 0 {
                    return 1;
                }
            }
            sleep(3);
            7
        }
        // a thread calls exit: the program ends with its code
        5 => {
            if start(exiter, 9) < 0 {
                return 1;
            }
            loop {
                thread::wait_word(&GATE, 0, 0);
            }
        }
        // a thread allowance of two: the third is refused, and a slot comes back when a thread has ended
        6 => {
            DONE[2].store(0, SeqCst);
            let a = start(releasable, 0);
            let b = start(sleeper, 0);
            let third = start(sleeper, 0);
            let mut code = 0;
            if a < 0 || b < 0 {
                code |= 1;
            }
            if third != -1 {
                code |= 2;
            }
            // when a thread has ended (joined), its place is free again at once
            RELEASE.store(1, Release);
            thread::wake_word(&RELEASE, 1);
            thread::join(&DONE[2]);
            if start(sleeper, 0) < 0 {
                code |= 4;
            }
            if start(sleeper, 0) != -1 {
                code |= 8;
            }
            code
        }
        // threads use the file system through the program's own session
        7 => {
            match Fs::open() {
                Ok(fs) => unsafe { *core::ptr::addr_of_mut!(FS) = Some(fs) },
                Err(e) => return 1000 + (-e) as u64,
            }
            CAPS.store(1 << 8, SeqCst);
            if start(reader, 0) < 0 {
                return 1;
            }
            thread::join(&DONE[3]);
            let theirs = RESULT.load(SeqCst);
            // ... and this task still can, with the descriptor numbering the thread's activity left behind
            LOCK.lock();
            let fs = unsafe { (*core::ptr::addr_of!(FS)).as_ref() }.unwrap();
            let mine = fs.open_file("/system/hello.txt", O_READ).and_then(|fd| {
                let mut buf = [0u8; 100];
                let n = fs.read(fd, &mut buf)?;
                fs.close(fd)?;
                Ok(n)
            });
            LOCK.unlock();
            match mine {
                Ok(n) if n as u32 == theirs && n > 0 => 0,
                Ok(n) => 500 + n as u64,
                Err(e) => 2000 + e as u64,
            }
        }
        // threads spin until the parent kills the program
        8 => {
            for _ in 0..4 {
                if start(spinner, 0) < 0 {
                    return 1;
                }
            }
            loop {
                SHARED.fetch_add(1, Relaxed);
            }
        }
        // many threads one after the other: slots and memory come back every time
        9 => {
            let stack = stack();
            let mut ok = true;
            for i in 0..150u32 {
                DONE[2].store(0, SeqCst);
                if thread::create(quick, stack, 0, 0) < 0 {
                    ok = false;
                    break;
                }
                thread::join(&DONE[2]);
                if SHARED.load(SeqCst) != i + 1 {
                    ok = false;
                    break;
                }
            }
            if ok { 0 } else { 1 }
        }
        // threads grow the program's heap: every page is different and visible to everybody
        10 => {
            for i in 0..4 {
                if start(heaper, i) < 0 {
                    return 1;
                }
            }
            for i in 0..4 {
                thread::join(&DONE[i]);
            }
            let mine = heap_grow(1);
            let pages: [u32; 4] = core::array::from_fn(|i| PAGES[i].load(SeqCst));
            let distinct = (0..4).all(|i| (0..4).all(|j| i == j || pages[i] != pages[j]) && pages[i] as i64 != mine);
            let seen = (0..4).all(|i| unsafe { core::ptr::read_volatile(pages[i] as *const u64) } == 0xABCD_0000 + i as u64);
            if SHARED.load(SeqCst) == 4 && mine > 0 && distinct && seen { 0 } else { 2 }
        }
        // a thread that ends naming a read-only page as its exit word cannot make the kernel write there
        11 => {
            let word = ro_exit as extern "C" fn(u64) -> ! as usize;
            let before = unsafe { core::ptr::read_volatile(word as *const u32) };
            if start(ro_exit, word as u64) < 0 {
                return 1;
            }
            while DONE[0].load(SeqCst) == 0 {
                sleep(1);
            }
            sleep(30); // (the thread is gone by now; the kernel would have stored its 1 on the way out)
            let after = unsafe { core::ptr::read_volatile(word as *const u32) };
            if before == after { 0 } else { 2 }
        }
        _ => 99,
    }
});
