//! The audio service (role 20) of a machine with no audio device: there is nothing to drive. (Such a machine never starts the role; a board that has a
//! device supplies its own `audiod.rs`.) The ring protocol and the clients are `audio.rs`.
use crate::*;

pub fn audiod() -> ! {
    println!("[audio] no audio device on this machine");
    loop {
        sleep(60_000);
    }
}
