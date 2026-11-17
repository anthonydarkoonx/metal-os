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

// SAFETY: `inner` (and every `Free` block the list reaches) is only touched with `lock` held (alloc, dealloc, free_bytes), so access from
// several threads is serialised; the blocks are the program's own heap memory.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap { lock: Mutex::new(), inner: UnsafeCell::new(Inner { head: null_mut() }) };

fn round(size: usize) -> usize {
    ((size + MIN - 1) & !(MIN - 1)).max(MIN)
}

impl Inner {
    /// Take `size` bytes (a multiple of 16) aligned to `align` (a power of two, at least 16) from the first block that fits.
    ///
    /// # Safety
    /// The heap lock must be held, and every block on the list must be a valid free block (16-aligned, size a multiple of 16, at least
    /// 16) inside heap memory this program owns, with `size` and `next` intact.
    unsafe fn take(&mut self, size: usize, align: usize) -> *mut u8 {
        // SAFETY: the caller holds the lock and every listed node is a valid free block inside heap memory we own, so reading and splitting nodes is valid; `at..at + size` lies inside block `cur`, so the new `Free` header written at the tail is in bounds.
        unsafe {
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
    }

    /// Put the block back in its place in the list, joining it with its neighbours.
    ///
    /// # Safety
    /// The heap lock must be held; `addr..addr + size` must be 16-aligned memory this program owns (`size` a multiple of 16, at least 16)
    /// that is neither in use nor already on the list.
    unsafe fn give(&mut self, addr: usize, size: usize) {
        // SAFETY: the caller holds the lock and guarantees `addr..addr + size` is 16-aligned memory we own that is not in use or listed, so writing a `Free` header there is valid; list nodes are valid free blocks (see `take`).
        unsafe {
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
    }

    /// # Safety
    /// The heap lock must be held and the list intact (see `take`); `l.size()` must be non-zero, as `GlobalAlloc` requires.
    unsafe fn alloc(&mut self, l: Layout) -> *mut u8 {
        // SAFETY: the caller holds the heap lock and the list is intact (this function's contract), so `take`/`give` see valid blocks; `heap_grow` returns fresh kernel-granted pages at `base`, which are 16-aligned, owned by this heap and not on the list.
        unsafe {
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
}

// SAFETY: blocks handed out are disjoint pieces of memory taken from the kernel with `HEAP_GROW`, aligned as asked (at least 16) and
// never touched again by the allocator until `dealloc` takes them back; all list work happens under the lock.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.lock.lock();
        // SAFETY: the lock is held, so this is the only access to `inner`; `GlobalAlloc` guarantees a non-zero size.
        let p = unsafe { (*self.inner.get()).alloc(l) };
        self.lock.unlock();
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        self.lock.lock();
        // SAFETY: the lock is held; by the `GlobalAlloc` contract `p`/`l` come from `alloc` of this heap, so the block is ours, 16-aligned, was
        // rounded the same way and is not on the list.
        unsafe { (*self.inner.get()).give(p as usize, round(l.size())) };
        self.lock.unlock();
    }
}

/// Bytes in the free list (for tests).
pub fn free_bytes() -> usize {
    HEAP.lock.lock();
    let mut n = 0;
    // SAFETY: the lock is held and the list is intact (every node is a free block this heap owns), so walking it reads valid nodes.
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
