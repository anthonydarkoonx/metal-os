//! Device-neutral input for the GUI: the events a program sees, a translator from the driver's packed words, and a gesture recogniser.
//! Specified in `docs/GUI-SPEC.md` (section 5).
//!
//! A mouse, a finger and a pen are all *pointers* (a mouse is id 0, touches are id 1 and up), so a program written for the mouse and keyboard
//! works unchanged with touch added later. Coordinates are logical pixels in 24.8 fixed point; time is milliseconds from any steady clock.
//! No floating point, no allocation, no clock of its own: everything is driven by the events and `tick` calls it is given, so it is tested on
//! the host with scripted traces.
#![forbid(unsafe_code)]
#![no_std]

pub mod driver;
pub mod event;
pub mod gesture;

pub use event::{Event, Fx, Key, KeyPhase, Mods, Phase, Pointer, PointerKind, Scroll, ScrollSource};
pub use gesture::{Gesture, Recognizer};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
