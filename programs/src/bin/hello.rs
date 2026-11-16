#![no_std]
#![no_main]
use metal_programs::*;

program!("hello", |c| {
    if c.arg == 41 {
        println!("[hello] I am a program started by task {} with argument {}", c.parent, c.arg);
    }
    if let Some(who) = c.argv(1) {
        println!("hello {who}");
    }
    c.arg
});
