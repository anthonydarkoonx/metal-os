//! Synchronisation built on `WAIT_WORD` / `WAKE_WORD`: `Mutex<T>`, `Condvar`, `RwLock`, `Once`/`OnceLock`, channels.
//! No poisoning (a panic ends the program).
pub use alloc::sync::{Arc, Weak};
pub use core::sync::atomic;

use crate::thread::{wait_word, wake_word, Mutex as RawMutex};
use alloc::collections::VecDeque;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;

pub struct Mutex<T: ?Sized> {
    raw: RawMutex,
    data: UnsafeCell<T>,
}

// SAFETY: the data is reachable only through a `MutexGuard`, which exists only while `raw` is held, so moving the mutex moves a `T: Send`.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
// SAFETY: the lock gives one thread at a time access to `data`, so sharing the mutex needs only `T: Send`.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

/// Not `Send` (the raw mutex is released by the thread that took it) and `Sync` only when `T` is, like std's.
pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
    _not_send: core::marker::PhantomData<*mut ()>,
}

// SAFETY: a `&MutexGuard` can only hand out `&T`, so sharing the guard between threads is sharing a `&T`, which `T: Sync` allows.
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Mutex<T> {
        Mutex { raw: RawMutex::new(), data: UnsafeCell::new(v) }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.raw.lock();
        MutexGuard { lock: self, _not_send: core::marker::PhantomData }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.raw.try_lock().then(|| MutexGuard { lock: self, _not_send: core::marker::PhantomData })
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a guard exists only while this thread holds `raw`, so no other reference to `data` exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the guard's only access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw.unlock();
    }
}

pub struct Condvar {
    seq: AtomicU32,
}

impl Condvar {
    pub const fn new() -> Condvar {
        Condvar { seq: AtomicU32::new(0) }
    }

    /// Unlock, sleep until notified, lock again. (Spurious wake-ups are possible: check the condition in a loop.)
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let seen = self.seq.load(Ordering::SeqCst);
        let m = guard.lock;
        drop(guard);
        wait_word(&self.seq, seen, 0);
        m.lock()
    }

    pub fn wait_while<'a, T>(&self, mut guard: MutexGuard<'a, T>, mut cond: impl FnMut(&mut T) -> bool) -> MutexGuard<'a, T> {
        while cond(&mut *guard) {
            guard = self.wait(guard);
        }
        guard
    }

    /// Like `wait`, with a limit; the flag says the time ran out.
    pub fn wait_timeout<'a, T>(&self, guard: MutexGuard<'a, T>, d: Duration) -> (MutexGuard<'a, T>, bool) {
        let seen = self.seq.load(Ordering::SeqCst);
        let m = guard.lock;
        drop(guard);
        let ticks = ((d.as_nanos() + 9_999_999) / 10_000_000).max(1) as u64;
        let r = wait_word(&self.seq, seen, ticks);
        (m.lock(), r == crate::thread::E_TIMEDOUT)
    }

    pub fn notify_one(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        wake_word(&self.seq, 1);
    }

    pub fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        wake_word(&self.seq, usize::MAX);
    }
}

/// Many readers or one writer.
pub struct RwLock<T> {
    state: Mutex<(u32, bool)>, // (readers, writer active)
    cv: Condvar,
    data: UnsafeCell<T>,
}

// SAFETY: moving the lock moves its `T`, which `T: Send` allows.
unsafe impl<T: Send> Send for RwLock<T> {}
// SAFETY: readers on several threads share `&T` (`T: Sync`) and a writer on any thread gets `&mut T` (`T: Send`); `state` keeps the two apart.
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

pub struct RwLockReadGuard<'a, T>(&'a RwLock<T>);
pub struct RwLockWriteGuard<'a, T>(&'a RwLock<T>);

impl<T> RwLock<T> {
    pub const fn new(v: T) -> RwLock<T> {
        RwLock { state: Mutex::new((0, false)), cv: Condvar::new(), data: UnsafeCell::new(v) }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let mut s = self.cv.wait_while(self.state.lock(), |s| s.1);
        s.0 += 1;
        RwLockReadGuard(self)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let mut s = self.cv.wait_while(self.state.lock(), |s| s.1 || s.0 > 0);
        s.1 = true;
        RwLockWriteGuard(self)
    }
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a read guard exists only while `state.0 > 0` was counted in `read`, and a writer cannot be active then, so only shared references exist.
        unsafe { &*self.0.data.get() }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a write guard exists only while `state.1` is set, which excludes every reader and any other writer.
        unsafe { &*self.0.data.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`: the write guard is the only access, and `&mut self` makes it exclusive.
        unsafe { &mut *self.0.data.get() }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock();
        s.0 -= 1;
        if s.0 == 0 {
            self.0.cv.notify_all();
        }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.0.state.lock().1 = false;
        self.0.cv.notify_all();
    }
}

/// Run something once.
pub struct Once {
    state: AtomicU32, // 0 new, 1 running, 2 done
}

impl Once {
    pub const fn new() -> Once {
        Once { state: AtomicU32::new(0) }
    }

    pub fn call_once(&self, f: impl FnOnce()) {
        loop {
            match self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
                Ok(_) => {
                    f();
                    self.state.store(2, Ordering::Release);
                    wake_word(&self.state, usize::MAX);
                    return;
                }
                Err(2) => return,
                Err(s) => {
                    wait_word(&self.state, s, 0);
                }
            }
        }
    }

    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }
}

/// A value set once, on first use.
pub struct OnceLock<T> {
    once: Once,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: `value` is written once, inside `call_once`, and is read only after `once` is complete (Release/Acquire); from then on it is only shared, so `T: Sync`, and `T: Send` because any thread may create it.
unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}
// SAFETY: moving the lock moves the (possibly set) `T`, which `T: Send` allows.
unsafe impl<T: Send> Send for OnceLock<T> {}

impl<T> OnceLock<T> {
    pub const fn new() -> OnceLock<T> {
        OnceLock { once: Once::new(), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    pub fn get(&self) -> Option<&T> {
        // SAFETY: `is_completed()` (Acquire) is true only after `call_once` wrote `value` and stored 2 (Release), so it is initialised and no longer written.
        self.once.is_completed().then(|| unsafe { (*self.value.get()).assume_init_ref() })
    }

    pub fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        // SAFETY: `call_once` runs this at most once and no reader looks at `value` before it completes, so the write to the uninitialised slot is exclusive.
        self.once.call_once(|| unsafe {
            (*self.value.get()).write(f());
        });
        self.get().unwrap()
    }
}

impl<T> Drop for OnceLock<T> {
    fn drop(&mut self) {
        if self.once.is_completed() {
            // SAFETY: `&mut self` rules out other references; `is_completed()` says `value` was initialised, and this drops it exactly once.
            unsafe { (*self.value.get()).assume_init_drop() };
        }
    }
}

/// Channels between threads.
pub mod mpsc {
    use super::*;

    struct State<T> {
        queue: VecDeque<T>,
        senders: usize,
        receiver: bool,
    }

    struct Chan<T> {
        state: Mutex<State<T>>,
        cv: Condvar,
    }

    pub struct Sender<T>(Arc<Chan<T>>);
    pub struct Receiver<T>(Arc<Chan<T>>);

    #[derive(Debug, PartialEq, Eq)]
    pub struct SendError<T>(pub T);
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub struct RecvError;
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub enum TryRecvError {
        Empty,
        Disconnected,
    }
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub enum RecvTimeoutError {
        Timeout,
        Disconnected,
    }

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let c = Arc::new(Chan { state: Mutex::new(State { queue: VecDeque::new(), senders: 1, receiver: true }), cv: Condvar::new() });
        (Sender(c.clone()), Receiver(c))
    }

    impl<T> Sender<T> {
        pub fn send(&self, v: T) -> Result<(), SendError<T>> {
            let mut s = self.0.state.lock();
            if !s.receiver {
                return Err(SendError(v));
            }
            s.queue.push_back(v);
            drop(s);
            self.0.cv.notify_one();
            Ok(())
        }
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Sender<T> {
            self.0.state.lock().senders += 1;
            Sender(self.0.clone())
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let mut s = self.0.state.lock();
            s.senders -= 1;
            if s.senders == 0 {
                drop(s);
                self.0.cv.notify_all();
            }
        }
    }

    impl<T> Receiver<T> {
        pub fn recv(&self) -> Result<T, RecvError> {
            let mut s = self.0.state.lock();
            loop {
                if let Some(v) = s.queue.pop_front() {
                    return Ok(v);
                }
                if s.senders == 0 {
                    return Err(RecvError);
                }
                s = self.0.cv.wait(s);
            }
        }

        pub fn try_recv(&self) -> Result<T, TryRecvError> {
            let mut s = self.0.state.lock();
            match s.queue.pop_front() {
                Some(v) => Ok(v),
                None if s.senders == 0 => Err(TryRecvError::Disconnected),
                None => Err(TryRecvError::Empty),
            }
        }

        pub fn recv_timeout(&self, d: Duration) -> Result<T, RecvTimeoutError> {
            let end = crate::time::Instant::now() + d;
            let mut s = self.0.state.lock();
            loop {
                if let Some(v) = s.queue.pop_front() {
                    return Ok(v);
                }
                if s.senders == 0 {
                    return Err(RecvTimeoutError::Disconnected);
                }
                let left = end.duration_since(crate::time::Instant::now());
                if left.is_zero() {
                    return Err(RecvTimeoutError::Timeout);
                }
                s = self.0.cv.wait_timeout(s, left).0;
            }
        }

        pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
            core::iter::from_fn(move || self.recv().ok())
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            self.0.state.lock().receiver = false;
        }
    }
}
