#![no_std]
#![no_main]
use metal_programs::fs::*;
use metal_programs::*;

// fswrite <path> <text>: writes the text to the file (created or replaced); exits 0, or 1000 + the error.
program!("fswrite", |c| {
    let (Some(path), Some(text)) = (c.argv(1), c.argv(2)) else { return 999 };
    let fs = match Fs::open() {
        Ok(fs) => fs,
        Err(e) => return 1000 + (-e) as u64,
    };
    match fs.write_file(path, text.as_bytes()) {
        Ok(()) => 0,
        Err(e) => 2000 + e as u64,
    }
});
