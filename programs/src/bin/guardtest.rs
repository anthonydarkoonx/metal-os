#![no_std]
#![no_main]
use metal_programs::*;

// Checks the guard pages and shows the randomised layout: `guardtest <mode>`.
//   0  print the layout: stack top, heap base, a ring page (exit 0)
//   1  write to the page below the stack (must fault)
//   2  read the page at the top of the stack (must fault)
//   3  recurse until the stack runs out (must fault)
//   4  the stack's first and last bytes are usable (exit 0)
//   5  read the page after the last heap page (must fault)
//   6  a thread on a stack the kernel made: it is in the stack region, apart from the main stack, usable at both ends,
//      charged to the heap limit while the thread lives and given back after it ends (exit 0)
//   7  a thread writes the page below its stack (must fault)
//   8  a thread reads the page above its stack (must fault)
//   9  a thread recurses until its stack runs out (must fault)
//   10 a stack over 1 MiB is refused (-5), one over the heap limit is refused (-8)
const PAGE: usize = 4096;
const STACK_PAGES: usize = 16;
const TSTACK_PAGES: usize = 8;

static DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static ACTION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SEEN_SP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SEEN_HELD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The thread: `ACTION` says what it does with its stack (it knows only where it is: the stack is `TSTACK_PAGES` long and
/// ends at the page boundary above its stack pointer).
extern "C" fn worker(_: u64) -> ! {
    use core::sync::atomic::Ordering::SeqCst;
    let here = core::hint::black_box(0u64);
    let sp = &here as *const u64 as usize;
    let top = (sp + PAGE) & !(PAGE - 1);
    SEEN_SP.store(top as u64, SeqCst);
    SEEN_HELD.store(quota().held, SeqCst);
    match ACTION.load(SeqCst) {
        // SAFETY: the first and last byte of the lowest page of the thread's own stack: mapped, writable, and far below anything the
        // thread's frames use (they are at the top).
        0 => unsafe {
            core::ptr::write_volatile((top - TSTACK_PAGES * PAGE) as *mut u8, 1);
            core::ptr::write_volatile((top - TSTACK_PAGES * PAGE + PAGE - 1) as *mut u8, 2);
        },
        // SAFETY: deliberately not sound: this is just below the thread's stack, in its guard page, so the write must fault and end the program; that is the test.
        1 => unsafe { core::ptr::write_volatile((top - TSTACK_PAGES * PAGE - 8) as *mut u64, 1) },
        2 => {
            // SAFETY: deliberately not sound: `top` is the first byte above the thread's stack (its guard page), so the read must fault; that is the test.
            let v = unsafe { core::ptr::read_volatile(top as *const u64) };
            println!("guardtest: the page above a thread's stack is readable ({v})");
        }
        _ => {
            let r = recurse(0);
            println!("guardtest: a thread's stack never ran out ({r})");
        }
    }
    thread::exit(&DONE)
}

#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let pad = [depth as u8; 512];
    let p = core::hint::black_box(&pad);
    if depth == u64::MAX {
        return p[0] as u64;
    }
    recurse(depth + 1) + p[(depth % 512) as usize] as u64
}

program!("guardtest", |c| -> u64 {
    let info = svc(SYSINFO, 9, 0, 0, 0, 0);
    let (heap_base, stack_top) = (info[1] as usize, info[2] as usize);
    match c.arg {
        0 => {
            let ring = svc(16, 0, 0, 0, 0, 0)[0];
            let here = &heap_base as *const usize as usize;
            let in_stack = here < stack_top && here >= stack_top - STACK_PAGES * PAGE;
            println!("layout stack_top={:#x} heap_base={:#x} ring={:#x} aslr={} rng={} local_in_stack={}", stack_top, heap_base, ring, info[3], info[4], in_stack);
            0
        }
        1 => {
            // SAFETY: deliberately not sound: this is the 8 bytes just below the main stack, in its guard page, so the write must fault; that is the test.
            unsafe { core::ptr::write_volatile((stack_top - (STACK_PAGES + 1) * PAGE + PAGE - 8) as *mut u64, 1) };
            println!("guardtest 1: the page below the stack is writable");
            0
        }
        2 => {
            // SAFETY: deliberately not sound: `stack_top` is the first byte above the main stack (its guard page), so the read must fault; that is the test.
            let v = unsafe { core::ptr::read_volatile(stack_top as *const u64) };
            println!("guardtest 2: the page at the top of the stack is readable ({v})");
            0
        }
        3 => {
            let r = recurse(0);
            println!("guardtest 3: the stack never ran out ({r})");
            0
        }
        4 => {
            // SAFETY: the first and last byte of the lowest page of the main stack: mapped, writable, and far below the frames in use.
            unsafe {
                core::ptr::write_volatile((stack_top - STACK_PAGES * PAGE) as *mut u8, 1);
                core::ptr::write_volatile((stack_top - STACK_PAGES * PAGE + PAGE - 1) as *mut u8, 2);
            }
            0
        }
        5 => {
            let p = heap_grow(1);
            if p < heap_base as i64 || p as usize % PAGE != 0 {
                println!("guardtest 5: heap_grow gave {p:#x}, heap base {heap_base:#x}");
                return 1;
            }
            // SAFETY: `p` is the page `heap_grow(1)` just gave us (checked >= heap base and page-aligned), so it is mapped and writable.
            unsafe { core::ptr::write_volatile(p as *mut u8, 1) };
            // SAFETY: deliberately not sound: the page after the last heap page is unmapped (docs/ABI.md, HEAP_GROW), so this read must fault; that is the test.
            let v = unsafe { core::ptr::read_volatile((p as usize + PAGE) as *const u8) };
            println!("guardtest 5: the page after the heap is readable ({v})");
            0
        }
        6..=9 => {
            use core::sync::atomic::Ordering::SeqCst;
            ACTION.store(c.arg - 6, SeqCst);
            let before = quota().held;
            let id = thread::create_with_kernel_stack(worker, 0, 0, TSTACK_PAGES * PAGE);
            if id < 0 {
                println!("guardtest {}: thread_create gave {id}", c.arg);
                return 1;
            }
            thread::join(&DONE);
            if c.arg != 6 {
                println!("guardtest {}: the thread's fault did not end the program", c.arg);
                return 0;
            }
            let (top, during, after) = (SEEN_SP.load(SeqCst) as usize, SEEN_HELD.load(SeqCst), quota().held);
            let main = (stack_top - (STACK_PAGES + 1) * PAGE, stack_top + PAGE);
            let (lo, hi) = (top - (TSTACK_PAGES + 1) * PAGE, top + PAGE);
            let apart = hi <= main.0 || lo >= main.1;
            let inside = lo >= 0x8040_0000 && hi <= 0x8100_0000;
            println!("tstack top={top:#x} held {before} -> {during} -> {after} apart={apart} inside={inside}");
            // and again: a new thread gets a (probably different) place and the first one's pages are really gone
            let first = top;
            DONE.store(0, SeqCst);
            thread::create_with_kernel_stack(worker, 0, 0, TSTACK_PAGES * PAGE);
            thread::join(&DONE);
            println!("second top={:#x}", SEEN_SP.load(SeqCst));
            let ok = apart && inside && during == before + TSTACK_PAGES as u64 && after == before && first % PAGE == 0;
            if ok { 0 } else { 1 }
        }
        10 => {
            let too_big = thread::create_with_kernel_stack(worker, 0, 0, (1 << 20) + 1);
            let over_quota = thread::create_with_kernel_stack(worker, 0, 0, 1 << 20); // (the test gives this program 64 pages)
            println!("tstack too_big={too_big} over_quota={over_quota}");
            0
        }
        _ => 2,
    }
});
