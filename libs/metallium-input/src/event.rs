//! The events programs receive.

use core::ops::{Add, Neg, Sub};

/// Logical pixels in 24.8 fixed point (1/256 of a pixel).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Fx(pub i32);

impl Fx {
    pub const ZERO: Fx = Fx(0);
    pub const fn from_int(v: i32) -> Fx {
        Fx(v.saturating_mul(256))
    }
    /// The whole pixel at or below this position.
    pub const fn floor(self) -> i32 {
        self.0 >> 8
    }
    /// The nearest whole pixel.
    pub const fn round(self) -> i32 {
        (self.0.saturating_add(128)) >> 8
    }
    pub const fn abs(self) -> Fx {
        Fx(self.0.saturating_abs())
    }
}
impl Add for Fx {
    type Output = Fx;
    fn add(self, o: Fx) -> Fx {
        Fx(self.0.saturating_add(o.0))
    }
}
impl Sub for Fx {
    type Output = Fx;
    fn sub(self, o: Fx) -> Fx {
        Fx(self.0.saturating_sub(o.0))
    }
}
impl Neg for Fx {
    type Output = Fx;
    fn neg(self) -> Fx {
        Fx(self.0.saturating_neg())
    }
}

/// Button bits.
pub const BTN_LEFT: u8 = 1;
pub const BTN_RIGHT: u8 = 2;
pub const BTN_MIDDLE: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerKind {
    Mouse,
    Touch,
    Pen,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Enter,
    Down,
    Move,
    Up,
    Cancel,
    Leave,
}

/// One pointer event. A touch `Down` implies the left button (`buttons` has `BTN_LEFT`), so a program that handles only the left button
/// works with touch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pointer {
    /// 0 for the mouse, 1 and up for touches and pens.
    pub id: u8,
    pub kind: PointerKind,
    pub phase: Phase,
    pub x: Fx,
    pub y: Fx,
    /// The buttons held after this event.
    pub buttons: u8,
    /// The button this `Down` or `Up` is about (0 for other phases).
    pub button: u8,
    /// 0 ..= 255; 0 when the device has none (a mouse reports 255 while a button is held).
    pub pressure: u8,
    /// Milliseconds.
    pub time: u32,
}

/// Modifier keys held.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;
    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit == bit
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyPhase {
    Down,
    Up,
    Repeat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    /// The key's number (Linux input codes, as the drivers report them).
    pub code: u16,
    /// The character the key produces with the active keymap, if any (none until a keymap is applied).
    pub ch: Option<char>,
    pub mods: Mods,
    pub phase: KeyPhase,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScrollSource {
    Wheel,
    Touchpad,
    Touch,
}

/// A scroll amount in logical pixels; positive `dy` means the content moves down (the user scrolls towards the top).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scroll {
    pub dx: Fx,
    pub dy: Fx,
    /// Wheel clicks, for programs that scroll by lines (0 for smooth sources).
    pub lines: i8,
    pub source: ScrollSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Pointer(Pointer),
    Key(Key),
    Scroll(Scroll),
    Focus(bool),
}
