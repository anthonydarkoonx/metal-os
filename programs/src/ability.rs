//! The ability model: how a program written for the program manager (`progmgr`) learns what state it is in.
//!
//! `progmgr` starts an ability as an ordinary program and gives it two capabilities: slot 0, a read-only *board* (one
//! shared page with a state word for every running ability) and slot 1, a send capability to the manager's
//! program-facing endpoint (which only accepts `save`). The numeric argument is the ability's board entry. The manager
//! changes the entry's state and wakes the ability with `WAKE_WORD`; the ability sleeps on the entry's sequence word
//! (`WAIT_WORD` with a timeout) and calls the matching method of [`Ability`]. Nothing is polled between changes.
//!
//! Lifecycle: `on_create` (once; given what the previous instance saved if it was restarted) → `on_foreground` /
//! `on_background` any number of times (UI abilities) → `on_destroy` (once, then the ability returns and ends). A service
//! ability never sees foreground or background. `on_tick` runs every `tick_interval()` ticks while the ability lives.
//! An ability that does not leave within the grace period after `on_destroy` was requested is killed.
use crate::sync::atomic::{AtomicU32, Ordering};
use crate::thread::wait_word;

/// Board and program-endpoint slots of an ability.
pub const BOARD_SLOT: u64 = 0;
pub const PROGRAM_EP_SLOT: u64 = 1;

pub const ST_STOPPED: u32 = 0;
/// A service ability that runs.
pub const ST_CREATED: u32 = 1;
pub const ST_FOREGROUND: u32 = 2;
pub const ST_BACKGROUND: u32 = 3;
/// `progmgr` has asked the ability to end.
pub const ST_DESTROYING: u32 = 4;
/// Too many crashes: `progmgr` gave up.
pub const ST_FAILED: u32 = 5;

/// Board layout: `ENTRIES` entries of `ENTRY` bytes.
pub const ENTRIES: usize = 16;
pub const ENTRY: usize = 32;
pub const O_STATE: usize = 0;
pub const O_SEQ: usize = 4;
pub const O_SAVED0: usize = 8;
pub const O_SAVED1: usize = 16;
pub const O_RESTORED: usize = 24;

/// The program-facing endpoint's one operation.
pub const OP_SAVE: u64 = 9;

pub trait Ability {
    fn on_create(&mut self, _restored: Option<(u64, u64)>) {}
    fn on_foreground(&mut self) {}
    fn on_background(&mut self) {}
    fn on_destroy(&mut self) {}
    fn on_tick(&mut self) {}
    fn tick_interval(&self) -> u64 {
        10
    }
    /// Checked after every callback: `true` makes the ability end (after `on_destroy`), as if the manager had stopped it.
    fn wants_exit(&self) -> bool {
        false
    }
}

/// Keep two words for the next instance of this ability: if it is restarted after a crash, `on_create` receives them.
/// (Kept in `progmgr`'s memory, so they survive a crash of the ability but not a restart of the manager.)
pub fn save(a: u64, b: u64) -> bool {
    matches!(crate::call(PROGRAM_EP_SLOT, [OP_SAVE, a, b]), (0, r) if r[0] == 0)
}

/// Exit codes of `run` when the program was not started by the program manager.
pub const NOT_MANAGED: u64 = 200;

pub fn run<A: Ability>(mut a: A, entry: u64) -> u64 {
    let base = crate::map_anywhere(BOARD_SLOT, false);
    if base < 0 || entry as usize >= ENTRIES {
        return NOT_MANAGED;
    }
    let e = (base as usize + entry as usize * ENTRY) as *const u8;
    // SAFETY: the board is one mapped page and `entry` is below ENTRIES; the words are only read here.
    let word = |o: usize| unsafe { &*(e.add(o) as *const AtomicU32) };
    let (state, seq) = (word(O_STATE), word(O_SEQ));
    let restored = (word(O_RESTORED).load(Ordering::Acquire) != 0).then(|| unsafe { (*(e.add(O_SAVED0) as *const u64), *(e.add(O_SAVED1) as *const u64)) });
    a.on_create(restored);
    let mut shown = 0u32;
    loop {
        if a.wants_exit() {
            a.on_destroy();
            return 0;
        }
        let s = seq.load(Ordering::Acquire);
        match state.load(Ordering::Acquire) {
            ST_DESTROYING | ST_STOPPED | ST_FAILED => {
                a.on_destroy();
                return 0;
            }
            st @ (ST_FOREGROUND | ST_BACKGROUND) if st != shown => {
                shown = st;
                if st == ST_FOREGROUND {
                    a.on_foreground()
                } else {
                    a.on_background()
                }
            }
            _ => {}
        }
        // sleep until the manager changes something or a tick is due
        if wait_word(seq, s, a.tick_interval().max(1)) == crate::thread::E_TIMEDOUT {
            a.on_tick();
        }
    }
}

/// `ability!("name", |ctx| Value)`: declare a program whose abilities implement [`Ability`].
#[macro_export]
macro_rules! ability {
    ($name:literal, $make:expr) => {
        $crate::program!($name, |c| $crate::ability::run(($make)(c), c.arg));
    };
}
