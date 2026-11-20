//! Translating the drivers' packed input words (`kbd.rs`, QEMU's virtio devices and the board's USB HID driver) into [`Event`]s.
//!
//! A word is `kind` (low byte), `code` (next 16 bits), `value` (high 32 bits). Kinds: `KEY` (value 1 pressed, 0 released, 2 repeat), `MOVE` (the
//! absolute position, `x | y << 16`, each 0 ..= 32767 of the device's range), `BUTTON` (code 1 left, 2 right, 3 middle), `WHEEL` (signed clicks).
//! Touch will be added as one more kind (`TOUCH`, reserved here as 5) without changing the others.

use crate::event::*;

pub const KEY: u64 = 1;
pub const MOVE: u64 = 2;
pub const BUTTON: u64 = 3;
pub const WHEEL: u64 = 4;
/// Reserved for the touch driver path.
pub const TOUCH: u64 = 5;

/// Pixels of scroll per wheel click.
pub const WHEEL_STEP_LP: i32 = 40;

/// The driver's absolute range.
pub const ABS_MAX: i32 = 32768;

// Key numbers are Linux input codes (what the virtio-input devices and the board's USB HID driver both report; `user/src/kbd.rs`).
pub const KEY_ESC: u16 = 1;
pub const KEY_TAB: u16 = 15;
pub const KEY_LCTRL: u16 = 29;
pub const KEY_LSHIFT: u16 = 42;
pub const KEY_RSHIFT: u16 = 54;
pub const KEY_LALT: u16 = 56;
pub const KEY_F4: u16 = 62;
pub const KEY_RCTRL: u16 = 97;
pub const KEY_RALT: u16 = 100;
pub const KEY_UP: u16 = 103;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_DOWN: u16 = 108;
pub const KEY_LSUPER: u16 = 125;
pub const KEY_RSUPER: u16 = 126;

/// A pointer position from the driver's 0 ..= 32767 range to logical pixels over `extent_lp`.
pub fn abs_to_fx(v: i32, extent_lp: i32) -> Fx {
    let v = v.clamp(0, ABS_MAX - 1) as i64;
    Fx((v * extent_lp.max(0) as i64 * 256 / ABS_MAX as i64) as i32)
}

/// The translator's state: where the pointer is, which buttons and modifiers are held.
#[derive(Clone, Debug)]
pub struct Translator {
    w_lp: i32,
    h_lp: i32,
    x: Fx,
    y: Fx,
    buttons: u8,
    mods: u8,
    inside: bool,
    /// Left-handed: the left and right buttons are swapped.
    swap: bool,
}

impl Translator {
    /// `w_lp` and `h_lp` are the screen size in logical pixels.
    pub fn new(w_lp: i32, h_lp: i32) -> Translator {
        Translator { w_lp, h_lp, x: Fx::ZERO, y: Fx::ZERO, buttons: 0, mods: 0, inside: false, swap: false }
    }

    pub fn set_size(&mut self, w_lp: i32, h_lp: i32) {
        self.w_lp = w_lp;
        self.h_lp = h_lp;
    }

    /// Swap the left and right buttons (a left-handed mouse). A button held while this changes is swapped too, so its release still matches.
    pub fn set_left_handed(&mut self, on: bool) {
        if on != self.swap {
            self.swap = on;
            let (l, r) = (self.buttons & BTN_LEFT != 0, self.buttons & BTN_RIGHT != 0);
            self.buttons &= !(BTN_LEFT | BTN_RIGHT);
            self.buttons |= if l { BTN_RIGHT } else { 0 } | if r { BTN_LEFT } else { 0 };
        }
    }

    pub fn left_handed(&self) -> bool {
        self.swap
    }

    pub fn pointer_at(&self) -> (Fx, Fx) {
        (self.x, self.y)
    }

    pub fn mods(&self) -> Mods {
        Mods(self.mods)
    }

    /// Feed one word; every resulting event goes to `out` (a move can give two: `Enter` the first time, then `Move`). Unknown kinds are ignored.
    pub fn feed(&mut self, word: u64, now_ms: u32, out: &mut dyn FnMut(Event)) {
        let (kind, code, value) = (word & 0xFF, (word >> 8) as u16, (word >> 32) as u32 as i32);
        match kind {
            MOVE => {
                self.x = abs_to_fx(value & 0xFFFF, self.w_lp);
                self.y = abs_to_fx((value >> 16) & 0xFFFF, self.h_lp);
                if !self.inside {
                    self.inside = true;
                    out(Event::Pointer(self.pointer(Phase::Enter, 0, now_ms)));
                }
                out(Event::Pointer(self.pointer(Phase::Move, 0, now_ms)));
            }
            BUTTON => {
                let bit = match (code, self.swap) {
                    (1, false) | (2, true) => BTN_LEFT,
                    (2, false) | (1, true) => BTN_RIGHT,
                    (3, _) => BTN_MIDDLE,
                    _ => return,
                };
                let down = value != 0;
                // a repeated press or release of a state already held is dropped (a driver may report both edges twice)
                if down == (self.buttons & bit != 0) {
                    return;
                }
                if down {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                out(Event::Pointer(self.pointer(if down { Phase::Down } else { Phase::Up }, bit, now_ms)));
            }
            WHEEL => {
                let clicks = value.clamp(-100, 100);
                if clicks != 0 {
                    out(Event::Scroll(Scroll {
                        dx: Fx::ZERO,
                        dy: Fx::from_int(clicks * WHEEL_STEP_LP),
                        lines: clicks as i8,
                        source: ScrollSource::Wheel,
                    }));
                }
            }
            KEY => {
                let phase = match value {
                    1 => KeyPhase::Down,
                    0 => KeyPhase::Up,
                    2 => KeyPhase::Repeat,
                    _ => return,
                };
                let bit = match code {
                    KEY_LSHIFT | KEY_RSHIFT => Mods::SHIFT,
                    KEY_LCTRL | KEY_RCTRL => Mods::CTRL,
                    KEY_LALT | KEY_RALT => Mods::ALT,
                    KEY_LSUPER | KEY_RSUPER => Mods::SUPER,
                    _ => 0,
                };
                // the modifier mask a key event carries includes the key itself on press and excludes it on release
                match phase {
                    KeyPhase::Down => self.mods |= bit,
                    KeyPhase::Up => self.mods &= !bit,
                    KeyPhase::Repeat => {}
                }
                out(Event::Key(Key { code, ch: None, mods: Mods(self.mods), phase }));
            }
            _ => {}
        }
    }

    fn pointer(&self, phase: Phase, button: u8, time: u32) -> Pointer {
        Pointer {
            id: 0,
            kind: PointerKind::Mouse,
            phase,
            x: self.x,
            y: self.y,
            buttons: self.buttons,
            button,
            pressure: if self.buttons != 0 { 255 } else { 0 },
            time,
        }
    }
}
