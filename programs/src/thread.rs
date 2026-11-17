//! Threads: tasks that share their program's address space. Created with `create` (the program must have been started
//! with a thread allowance), joined through a word the kernel stores into and wakes when the thread ends.
//! `wait_word` / `wake_word` are the only blocking primitive; `Mutex` is built from them.
use crate::svc;
use core::sync::atomic::{AtomicU32, Ordering};

pub const THREAD_CREATE: u64 = 30;
pub const THREAD_EXIT: u64 = 31;
pub const WAIT_WORD: u64 = 32;
pub const WAKE_WORD: u64 = 33;
pub const E_AGAIN: i64 = -10;
pub const E_TIMEDOUT: i64 = -9;

/// Start a thread at `entry(arg)` on the stack that ends at `stack_top` (16-byte aligned). It gets copies of the
/// capabilities in the slots named by the bits of `caps`. Returns its task id, or a negative error.
///
/// # Safety
/// `stack_top` must be the 16-byte aligned end of memory this program owns, large enough for the thread and used by no one
/// else until the thread has ended (the kernel cannot tell a good address from a bad one).
pub unsafe fn create(entry: extern "C" fn(u64) -> !, stack_top: usize, arg: u64, caps: u32) -> i64 {
    svc(THREAD_CREATE, entry as usize as u64, stack_top as u64, arg, caps as u64, 0)[0] as i64
}

/// Start a thread at `entry(arg)` on a stack the kernel makes: `bytes` (0 = 64 KiB, at most 1 MiB) rounded up to pages, at a
/// random place with an unmapped page on each side, charged to the program's heap limit and given back when the thread ends.
/// Otherwise like `create`.
pub fn create_with_kernel_stack(entry: extern "C" fn(u64) -> !, arg: u64, caps: u32, bytes: usize) -> i64 {
    svc(THREAD_CREATE, entry as usize as u64, 0, arg, caps as u64, bytes as u64)[0] as i64
}

/// End the calling thread; the kernel first stores 1 in `done` and wakes everyone waiting on it (see `join`).
pub fn exit(done: &AtomicU32) -> ! {
    svc(THREAD_EXIT, done as *const AtomicU32 as u64, 1, 0, 0, 0);
    loop {}
}

/// Wait until `done` is non-zero, i.e. the thread that was told to end through it has ended.
pub fn join(done: &AtomicU32) {
    while done.load(Ordering::Acquire) == 0 {
        wait_word(done, 0, 0);
    }
}

/// Sleep while `*word == expected`: 0 when woken, `E_AGAIN` if the word differed, `E_TIMEDOUT` after `timeout` ticks
/// (0 = no limit).
pub fn wait_word(word: &AtomicU32, expected: u32, timeout: u64) -> i64 {
    svc(WAIT_WORD, word as *const AtomicU32 as u64, expected as u64, timeout, 0, 0)[0] as i64
}

/// Wake up to `count` threads sleeping on `word`; how many were.
pub fn wake_word(word: &AtomicU32, count: usize) -> i64 {
    svc(WAKE_WORD, word as *const AtomicU32 as u64, count as u64, 0, 0, 0)[0] as i64
}

/// A lock: 0 free, 1 held, 2 held with sleepers.
pub struct Mutex(AtomicU32);

impl Mutex {
    pub const fn new() -> Mutex {
        Mutex(AtomicU32::new(0))
    }

    pub fn lock(&self) {
        if self.0.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }
        // contended: say so (2) and sleep until it is released; whoever takes it next must keep saying so
        while self.0.swap(2, Ordering::Acquire) != 0 {
            wait_word(&self.0, 2, 0);
        }
    }

    pub fn try_lock(&self) -> bool {
        self.0.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    pub fn unlock(&self) {
        if self.0.swap(0, Ordering::Release) == 2 {
            wake_word(&self.0, 1);
        }
    }
}

// ---- the std-like layer: closures, handles, scopes ----------------------------------------------------------------------

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::time::Duration;

/// Default stack of a thread: made by the kernel and charged to the program's heap limit, so size that in the spawn request.
pub const DEFAULT_STACK: usize = 64 * 1024;

struct Packet<T> {
    done: AtomicU32,
    result: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send> Sync for Packet<T> {}
unsafe impl<T: Send> Send for Packet<T> {}

pub struct JoinHandle<T> {
    packet: Option<Arc<Packet<T>>>,
    id: i64,
}

impl<T> JoinHandle<T> {
    /// The thread's task id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Wait for the thread to end and take what it returned.
    pub fn join(mut self) -> T {
        let p = self.packet.take().unwrap();
        join(&p.done);
        unsafe { (*p.result.get()).take().expect("a thread ends only by returning") }
    }

    pub fn is_finished(&self) -> bool {
        self.packet.as_ref().map_or(true, |p| p.done.load(Ordering::Acquire) != 0)
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // a thread nobody waits for goes on running on a stack and a result slot that must stay: they are given up
        if let Some(p) = self.packet.take() {
            core::mem::forget(p);
        }
    }
}

/// What the new task starts with: the job, and the word the kernel stores into when the task is gone.
struct Start {
    job: Box<dyn FnOnce()>,
    done: *const AtomicU32,
}

extern "C" fn trampoline(arg: u64) -> ! {
    let Start { job, done } = *unsafe { Box::from_raw(arg as *mut Start) };
    job();
    exit(unsafe { &*done })
}

pub struct Builder {
    stack_size: usize,
    caps: u32,
}

impl Builder {
    pub fn new() -> Builder {
        Builder { stack_size: DEFAULT_STACK, caps: !0 }
    }

    pub fn stack_size(mut self, bytes: usize) -> Builder {
        self.stack_size = bytes.max(4096).min(1 << 20);
        self
    }

    /// Which of the capabilities of the creator the thread gets copies of: bit `i` = slot `i` (default: all; empty
    /// slots are skipped).
    pub fn capabilities(mut self, mask: u32) -> Builder {
        self.caps = mask;
        self
    }

    pub fn spawn<F, T>(self, f: F) -> crate::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        unsafe { self.spawn_unchecked(f) }
    }

    /// # Safety
    /// The caller guarantees `f` and its result outlive the thread (scoped threads do).
    pub unsafe fn spawn_unchecked<F, T>(self, f: F) -> crate::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send,
        T: Send,
    {
        let (packet, id) = unsafe { self.start(f)? };
        Ok(JoinHandle { packet: Some(packet), id })
    }

    unsafe fn start<F, T>(self, f: F) -> crate::io::Result<(Arc<Packet<T>>, i64)>
    where
        F: FnOnce() -> T + Send,
        T: Send,
    {
        let packet = Arc::new(Packet { done: AtomicU32::new(0), result: UnsafeCell::new(None) });
        let p2 = packet.clone();
        let job: Box<dyn FnOnce() + '_> = Box::new(move || {
            let r = f();
            unsafe { *p2.result.get() = Some(r) };
            // (dropping our share cannot free the packet: whoever started the thread keeps one until it has been joined)
        });
        let job: Box<dyn FnOnce()> = unsafe { core::mem::transmute(job) };
        let start = Box::into_raw(Box::new(Start { job, done: &packet.done }));
        let id = create_with_kernel_stack(trampoline, start as u64, self.caps, self.stack_size);
        if id < 0 {
            drop(unsafe { Box::from_raw(start) });
            return Err(crate::io::Error::from_kernel(id));
        }
        Ok((packet, id))
    }
}

pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    Builder::new().spawn(f).expect("cannot start a thread (no allowance, or no free task slot)")
}

pub fn yield_now() {
    crate::yield_now();
}

pub fn sleep(d: Duration) {
    crate::time::sleep(d);
}

/// Threads that may borrow from the caller; all are joined before `scope` returns.
pub struct Scope<'scope, 'env: 'scope> {
    /// The word each thread's end is announced on (the packets live until the scope has seen them all end).
    ends: crate::sync::Mutex<Vec<Arc<dyn Ended>>>,
    _marker: core::marker::PhantomData<(&'scope mut &'scope (), &'env mut &'env ())>,
}

trait Ended: Send + Sync {
    fn word(&self) -> &AtomicU32;
}

impl<T: Send> Ended for Packet<T> {
    fn word(&self) -> &AtomicU32 {
        &self.done
    }
}

pub struct ScopedJoinHandle<'scope, T> {
    packet: Arc<Packet<T>>,
    _marker: core::marker::PhantomData<&'scope ()>,
}

impl<T: Send> ScopedJoinHandle<'_, T> {
    pub fn join(self) -> T {
        join(&self.packet.done);
        unsafe { (*self.packet.result.get()).take().expect("a thread ends only by returning") }
    }
}

impl<'scope, 'env> Scope<'scope, 'env> {
    pub fn spawn<F, T>(&'scope self, f: F) -> ScopedJoinHandle<'scope, T>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        match unsafe { Builder::new().start(f) } {
            Ok((packet, _)) => {
                let ended: Arc<dyn Ended> = unsafe { core::mem::transmute::<Arc<dyn Ended + '_>, Arc<dyn Ended>>(packet.clone() as Arc<dyn Ended + '_>) };
                self.ends.lock().push(ended);
                ScopedJoinHandle { packet, _marker: core::marker::PhantomData }
            }
            Err(e) => panic!("cannot start a thread: {e}"),
        }
    }
}

pub fn scope<'env, F, T>(f: F) -> T
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
{
    let s = Scope { ends: crate::sync::Mutex::new(Vec::new()), _marker: core::marker::PhantomData };
    let r = f(&s);
    // A scoped thread may itself spawn more on the same scope, so one pass is not enough: keep joining until a pass finds nothing new.
    // Any thread that can still spawn is itself in the list, so it has been joined before the list is found empty.
    loop {
        let ends = core::mem::take(&mut *s.ends.lock());
        if ends.is_empty() {
            break;
        }
        for e in ends {
            join(e.word());
        }
    }
    r
}
