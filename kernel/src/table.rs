//! Tables whose length is chosen at boot (the per-task state, the capability nodes). The kernel has no heap, so each table is carved
//! out of RAM once, from contiguous frames, and lives for the kernel's whole life.
//!
//! `Table<T>` hands out shared references only: its elements are atomics, locks or cells (anything `Sync` with interior mutability),
//! exactly as the `static [T; N]` arrays it replaces were.
use crate::mem::{self, PAGE};
use core::ops::Index;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub struct Table<T> {
    ptr: AtomicPtr<T>,
    len: AtomicUsize,
}

// SAFETY: a `Table` only gives out `&T`, and `T: Sync` makes sharing those between cores sound; the storage is never freed or moved.
unsafe impl<T: Sync> Sync for Table<T> {}

impl<T> Table<T> {
    pub const fn new() -> Self {
        Table { ptr: AtomicPtr::new(core::ptr::null_mut()), len: AtomicUsize::new(0) }
    }

    /// Carve `n` elements out of RAM, element `i` made by `f(i)`. Once only, and only after the allocator is up.
    pub fn init(&self, n: usize, mut f: impl FnMut(usize) -> T) {
        assert!(n > 0 && self.len.load(Ordering::Acquire) == 0, "table: initialised twice, or empty");
        assert!(core::mem::align_of::<T>() <= PAGE && core::mem::size_of::<T>() > 0, "table: unsupported element type");
        let bytes = n * core::mem::size_of::<T>();
        let pa = mem::alloc_contig(bytes.div_ceil(PAGE), mem::OWNER_KERNEL).expect("table: out of memory");
        let end = pa + bytes;
        // (one pointer must reach the whole run: it may not straddle the identity window and the direct map)
        assert!(end <= crate::frame::phys::ID_END || pa >= crate::frame::phys::ID_END, "table: run straddles the direct-map boundary");
        let p = crate::frame::phys::va(pa) as *mut T;
        for i in 0..n {
            // SAFETY: `p..p+n` is inside the frames just taken (zeroed, contiguous, owned by nobody else, in one mapping); each
            // element is written once before the table is published, and `p` is aligned (page-aligned base, align_of::<T>() <= PAGE).
            unsafe { p.add(i).write(f(i)) };
        }
        self.ptr.store(p, Ordering::Release);
        self.len.store(n, Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        let n = self.len();
        if n == 0 {
            return [].iter();
        }
        // SAFETY: `init` wrote `n` elements at `ptr` before publishing `len` (acquire/release pair), and they live for ever.
        unsafe { core::slice::from_raw_parts(self.ptr.load(Ordering::Acquire), n) }.iter()
    }
}

impl<T> Index<usize> for Table<T> {
    type Output = T;
    #[inline]
    fn index(&self, i: usize) -> &T {
        let n = self.len.load(Ordering::Acquire);
        assert!(i < n, "table: index {i} out of {n}");
        // SAFETY: `i < len`, and `init` wrote every element before publishing `len`; the storage lives for ever.
        unsafe { &*self.ptr.load(Ordering::Acquire).add(i) }
    }
}
