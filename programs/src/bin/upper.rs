#![no_std]
#![no_main]
use metallium_programs::*;

// Copies standard input to standard output in capitals, line by line; exits with the number of lines.
program!("upper", |_c| {
    let mut line = [0u8; 100];
    let mut n = 0;
    while let Some(len) = read_line(&mut line) {
        for b in line[..len].iter_mut() {
            b.make_ascii_uppercase();
        }
        write_out(&line[..len]);
        write_out(b"\n");
        n += 1;
    }
    n
});
