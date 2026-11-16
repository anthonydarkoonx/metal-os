//! The program's allocator: a first-fit free list in address order, with coalescing, over memory taken from the heap
//! window with `HEAP_GROW` in chunks of at least 64 KiB. One lock (a `thread::Mutex`, so it costs one atomic operation
//! until two threads collide). Memory is not returned to the kernel; it is reused.
use crate::thread::Mutex;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;

static GROWN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// How much memory the heap has taken from the kernel so far (it is never given back).
pub fn grown_bytes() -> usize {
    GROWN.load(core::sync::atomic::Ordering::Relaxed)
}

const MIN: usize = 16;
const GROW_PAGES: usize = 16;
const PAGE: usize = 4096;

#[repr(C)]
struct Free {
    size: usize,
    next: *mut Free,
}

struct Inner {
    head: *mut Free,
}

pub struct Heap {
    lock: Mutex,
    inner: UnsafeCell<Inner>,
}

unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap { lock: Mutex::new(), inner: UnsafeCell::new(Inner { head: null_mut() }) };

fn round(size: usize) -> usize {
    ((size + MIN - 1) & !(MIN - 1)).max(MIN)
}

impl Inner {
    /// Take `size` bytes (a multiple of 16) aligned to `align` (a power of two, at least 16) from the first block that fits.
    unsafe fn take(&mut self, size: usize, align: usize) -> *mut u8 {
        let mut link: *mut *mut Free = &mut self.head;
        let mut cur = self.head;
        while !cur.is_null() {
            let start = cur as usize;
            let end = start + (*cur).size;
            let at = (start + align - 1) & !(align - 1);
            if at + size <= end {
                // (start and at are multiples of 16, so the gap in front and the tail behind are each 0 or a block)
                let (gap, tail) = (at - start, end - (at + size));
                let next = (*cur).next;
                let mut after = next;
                if tail > 0 {
                    let t = (at + size) as *mut Free;
                    (*t).size = tail;
                    (*t).next = next;
                    after = t;
                }
                if gap > 0 {
                    (*cur).size = gap;
                    (*cur).next = after;
                } else {
                    *link = after;
                }
                return at as *mut u8;
            }
            link = &mut (*cur).next;
            cur = (*cur).next;
        }
        null_mut()
    }

    /// Put the block back in its place in the list, joining it with its neighbours.
    unsafe fn give(&mut self, addr: usize, size: usize) {
        let mut prev: *mut Free = null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < addr {
            prev = cur;
            cur = (*cur).next;
        }
        let node = addr as *mut Free;
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

    unsafe fn alloc(&mut self, l: Layout) -> *mut u8 {
        let size = round(l.size());
        let align = l.align().max(MIN);
        loop {
            let p = self.take(size, align);
            if !p.is_null() {
                return p;
            }
            let pages = ((size + align + PAGE - 1) / PAGE).max(GROW_PAGES);
            let base = crate::heap_grow(pages as u64);
            if base <= 0 {
                return null_mut();
            }
            GROWN.fetch_add(pages * PAGE, core::sync::atomic::Ordering::Relaxed);
            self.give(base as usize, pages * PAGE);
        }
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.lock.lock();
        let p = (*self.inner.get()).alloc(l);
        self.lock.unlock();
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        self.lock.lock();
        (*self.inner.get()).give(p as usize, round(l.size()));
        self.lock.unlock();
    }
}

/// Bytes in the free list (for tests).
pub fn free_bytes() -> usize {
    HEAP.lock.lock();
    let mut n = 0;
    unsafe {
        let mut cur = (*HEAP.inner.get()).head;
        while !cur.is_null() {
            n += (*cur).size;
            cur = (*cur).next;
        }
    }
    HEAP.lock.unlock();
    n
}
