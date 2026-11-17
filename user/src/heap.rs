//! Per-task heap: first-fit free list with address-ordered coalescing, grown
//! on demand with the HEAP_GROW syscall (kernel-owned frames, reclaimed when
//! the task dies). Tasks are single-threaded, so no locking.
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;

const HEAP_GROW: u64 = 15;
const PAGE: usize = 4096;
const MIN_GROW_PAGES: usize = 64;

struct Node {
    size: usize,
    next: *mut Node,
}

struct Inner {
    head: *mut Node,
}

pub struct Heap(UnsafeCell<Inner>);

// SAFETY: a task's programs are single-threaded (this image never creates threads, and `THREAD_CREATE` allowance defaults to 0), so the allocator is never entered concurrently.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(Inner { head: null_mut() }));

fn round(n: usize) -> usize {
    (n.max(16) + 15) & !15
}

impl Inner {
    /// Insert a free region, keeping the list address-ordered and merged.
    /// # Safety
    /// `addr..addr + size` must be a free, 16-byte-aligned region of at least 16 bytes owned by this heap (fresh HEAP_GROW pages or a block returned to `dealloc`) and not already on the list.
    unsafe fn insert(&mut self, addr: usize, size: usize) {
        let mut prev: *mut Node = null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < addr {
            prev = cur;
            cur = (*cur).next;
        }
        let node = addr as *mut Node;
        (*node).size = size;
        (*node).next = cur;
        if !cur.is_null() && addr + size == cur as usize {
            (*node).size += (*cur).size;
            (*node).next = (*cur).next;
        }
        if prev.is_null() {
            self.head = node;
        } else if prev as usize + (*prev).size == addr {
            (*prev).size += (*node).size;
            (*prev).next = (*node).next;
        } else {
            (*prev).next = node;
        }
    }

    /// # Safety
    /// Must be called with exclusive access to the allocator (single-threaded task); it hands the new kernel-mapped pages to `insert`.
    unsafe fn grow(&mut self, need: usize) -> bool {
        let pages = need.div_ceil(PAGE).max(MIN_GROW_PAGES);
        let r: i64;
        // the kernel hands results back in x0..x5, so every argument register is declared as possibly changed
        core::arch::asm!("svc #0", inout("x0") HEAP_GROW => r, inout("x1") pages as u64 => _, lateout("x2") _, lateout("x3") _, lateout("x4") _, lateout("x5") _, options(nostack));
        if r < 0 {
            return false;
        }
        self.insert(r as usize, pages * PAGE);
        true
    }
}

// SAFETY: `alloc` returns 16-aligned blocks cut from kernel-granted heap pages (align > 16 is refused) and removes them from the free list; `dealloc` takes them back with the same rounding, so blocks never overlap.
unsafe impl GlobalAlloc for Heap {
    /// # Safety
    /// Trait contract of `GlobalAlloc::alloc`; the free list is only touched here, with the task being single-threaded.
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.align() > 16 {
            return null_mut();
        }
        let size = round(l.size());
        let h = &mut *self.0.get();
        loop {
            let mut prev: *mut *mut Node = &mut h.head;
            let mut cur = h.head;
            while !cur.is_null() {
                if (*cur).size >= size {
                    let rem = (*cur).size - size;
                    if rem >= 16 {
                        let n = (cur as usize + size) as *mut Node;
                        (*n).size = rem;
                        (*n).next = (*cur).next;
                        *prev = n;
                    } else {
                        *prev = (*cur).next;
                    }
                    return cur as *mut u8;
                }
                prev = &mut (*cur).next;
                cur = (*cur).next;
            }
            if !h.grow(size) {
                return null_mut();
            }
        }
    }

    /// # Safety
    /// `p` must come from `alloc` with the same `l`; the block is re-inserted with the same rounded size.
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        (*self.0.get()).insert(p as usize, round(l.size()));
    }
}
