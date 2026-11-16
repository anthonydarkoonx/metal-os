#![no_std]
#![no_main]
use metal_programs::*;

// Prints its command line and a few environment variables; exits with the number of arguments.
program!("argdump", |c| {
    println!("argc={}", c.argc());
    let mut i = 0;
    while let Some(a) = c.argv(i) {
        println!("argv[{i}]={a}");
        i += 1;
    }
    println!("PWD={}", c.env("PWD").unwrap_or("-"));
    println!("TERM={}", c.env("TERM").unwrap_or("-"));
    println!("arg={} parent={}", c.arg, c.parent);
    c.argc() as u64
});
