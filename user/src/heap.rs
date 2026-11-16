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

unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(Inner { head: null_mut() }));

fn round(n: usize) -> usize {
    (n.max(16) + 15) & !15
}

impl Inner {
    /// Insert a free region, keeping the list address-ordered and merged.
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

    unsafe fn grow(&mut self, need: usize) -> bool {
        let pages = need.div_ceil(PAGE).max(MIN_GROW_PAGES);
        let r: i64;
        core::arch::asm!("svc #0", inout("x0") HEAP_GROW => r, in("x1") pages as u64, options(nostack));
        if r < 0 {
            return false;
        }
        self.insert(r as usize, pages * PAGE);
        true
    }
}

unsafe impl GlobalAlloc for Heap {
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

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        (*self.0.get()).insert(p as usize, round(l.size()));
    }
}
