#![no_std]
#![no_main]
use metallium_programs::fs::*;
use metallium_programs::*;

// File descriptors belong to one program. `fdprobe open`: opens a file (fd 0) and waits to be killed.
// `fdprobe read`: tries to read fd 0 without opening anything, which must fail with EBADF (3); exits with the status
// (so 3 is right), or 100 if the read worked.
program!("fdprobe", |c| {
    let fs = match Fs::open() {
        Ok(fs) => fs,
        Err(e) => return 1000 + (-e) as u64,
    };
    match c.argv(1) {
        Some("open") => {
            let _ = fs.open_file("/system/hello.txt", O_READ);
            loop {
                sleep(10);
            }
        }
        _ => {
            let mut b = [0u8; 8];
            match fs.read(0, &mut b) {
                Ok(_) => 100,
                Err(e) => e as u64,
            }
        }
    }
});
