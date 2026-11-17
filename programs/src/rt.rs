//! Start-up state saved for the library (`env`, `process`): what the program was started with.
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ARGS_PTR: AtomicU64 = AtomicU64::new(0);
static ARGS_LEN: AtomicU64 = AtomicU64::new(0);
static ARG: AtomicU64 = AtomicU64::new(0);
static PARENT: AtomicU64 = AtomicU64::new(0);

#[doc(hidden)]
pub fn init(arg: u64, parent: u64, args_ptr: u64, args_len: u64) {
    ARG.store(arg, Relaxed);
    PARENT.store(parent, Relaxed);
    ARGS_PTR.store(args_ptr, Relaxed);
    ARGS_LEN.store(args_len, Relaxed);
}

/// (argument block address, length, numeric argument, parent)
pub fn saved() -> (u64, u64, u64, u64) {
    (ARGS_PTR.load(Relaxed), ARGS_LEN.load(Relaxed), ARG.load(Relaxed), PARENT.load(Relaxed))
}

/// What a program's `main` may return: the exit code.
pub trait Termination {
    fn report(self) -> u64;
}

impl Termination for u64 {
    fn report(self) -> u64 {
        self
    }
}

impl Termination for () {
    fn report(self) -> u64 {
        0
    }
}

impl<E: core::fmt::Debug> Termination for Result<(), E> {
    fn report(self) -> u64 {
        match self {
            Ok(()) => 0,
            Err(e) => {
                crate::eprintln!("error: {e:?}");
                1
            }
        }
    }
}

/// What `program!` calls: record the start-up state, run `main`, exit with what it returns.
///
/// # Safety
/// The arguments must be what the kernel passed to `_start` (see `Ctx::new`).
#[doc(hidden)]
pub unsafe fn start<R: Termination>(main: fn(&crate::Ctx) -> R, arg: u64, parent: u64, args_ptr: u64, args_len: u64) -> ! {
    init(arg, parent, args_ptr, args_len);
    // SAFETY: `start`'s own contract is that these are the values the kernel passed to `_start`, which is what `Ctx::new` requires.
    crate::exit(main(&unsafe { crate::Ctx::new(arg, parent, args_ptr, args_len) }).report())
}
