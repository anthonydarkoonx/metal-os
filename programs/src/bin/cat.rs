#![no_std]
#![no_main]
use metallium_programs::fs::*;
use metallium_programs::*;

// cat <path>: prints a file through the VFS; exits with the number of bytes, or 1000 + the error.
program!("cat", |c| {
    let Some(path) = c.argv(1) else { return 999 };
    let fs = match Fs::open() {
        Ok(fs) => fs,
        Err(e) => return 1000 + (-e) as u64,
    };
    let fd = match fs.open_file(path, O_READ) {
        Ok(fd) => fd,
        Err(e) => return 2000 + e as u64,
    };
    let mut buf = [0u8; 200];
    let mut total = 0u64;
    loop {
        match fs.read(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                write_out(&buf[..n]);
            }
            Err(e) => return 3000 + e as u64,
        }
    }
    let _ = fs.close(fd);
    total
});
