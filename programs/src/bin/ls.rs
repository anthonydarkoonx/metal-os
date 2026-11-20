#![no_std]
#![no_main]
use metallium_programs::fs::*;
use metallium_programs::*;

// ls <dir>: prints the names; exits with their number.
program!("ls", |c| {
    let path = c.argv(1).unwrap_or("/");
    let fs = match Fs::open() {
        Ok(fs) => fs,
        Err(e) => return 1000 + (-e) as u64,
    };
    let mut buf = [0u8; 200];
    match fs.list(path, &mut buf) {
        Ok(n) => {
            write_out(&buf[..n]);
            println!("");
            buf[..n].split(|&b| b == b'\n').filter(|s| !s.is_empty()).count() as u64
        }
        Err(e) => 2000 + e as u64,
    }
});
