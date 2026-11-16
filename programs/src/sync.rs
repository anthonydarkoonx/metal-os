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

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
}

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
        MutexGuard { lock: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.raw.try_lock().then(|| MutexGuard { lock: self })
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
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

unsafe impl<T: Send> Send for RwLock<T> {}
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
        unsafe { &*self.0.data.get() }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.0.data.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
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

unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}
unsafe impl<T: Send> Send for OnceLock<T> {}

impl<T> OnceLock<T> {
    pub const fn new() -> OnceLock<T> {
        OnceLock { once: Once::new(), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    pub fn get(&self) -> Option<&T> {
        self.once.is_completed().then(|| unsafe { (*self.value.get()).assume_init_ref() })
    }

    pub fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        self.once.call_once(|| unsafe {
            (*self.value.get()).write(f());
        });
        self.get().unwrap()
    }
}

impl<T> Drop for OnceLock<T> {
    fn drop(&mut self) {
        if self.once.is_completed() {
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
