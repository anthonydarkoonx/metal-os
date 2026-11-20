#![no_std]
#![no_main]
extern crate alloc;
use alloc::format;
use alloc::string::String;
use metallium_programs::process::Command;
use metallium_programs::*;

// What a session sees (started by `sessiond`, confined to the user's home). argv[1] = the user, argv[2] = "probe" or "child". Prints `[homeprobe <user>] key: value`
// lines that `make session-test` checks. The probe starts a child (same program, "child") with the spawn right it was given: the child must be confined to the
// same home, not given an unconfined file session.
fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

program!("homeprobe", |c| {
    let user = String::from(c.argv(1).unwrap_or("?"));
    let mode = c.argv(2).unwrap_or("probe");
    let tag = if mode == "child" { format!("{user}/child") } else { user.clone() };
    println!("[homeprobe {tag}] started");
    // its own files
    let note = format!("note of {user}");
    let wrote = fs::write("/data/note.txt", note.as_bytes()).is_ok() && fs::read("/data/note.txt").map_or(false, |b| b == note.as_bytes());
    println!("[homeprobe {tag}] own file written and read back: {}", yes(wrote));
    // what it cannot see: the account store (a real file in the real /data), other homes, the parent directory
    println!("[homeprobe {tag}] sees the account store: {}", yes(fs::read("/data/accounts").is_ok() || fs::read_dir("/data/home").is_ok()));
    println!("[homeprobe {tag}] can name .. : {}", yes(fs::read("/data/../home").is_ok() || fs::read("/data/../accounts").is_ok()));
    println!("[homeprobe {tag}] can name /home or /accounts directly: {}", yes(fs::read("/home").is_ok() || fs::read("/accounts").is_ok() || fs::read_dir("/home").is_ok()));
    let listing = fs::read_dir("/data").map(|l| l.iter().map(|e| e.name.clone()).collect::<alloc::vec::Vec<_>>()).unwrap_or_default();
    println!("[homeprobe {tag}] listing of /data: {}", listing.join(" "));
    if mode == "child" {
        let _ = fs::write("/data/child.txt", format!("child of {user}").as_bytes());
        println!("[homeprobe {tag}] sees the parent's note: {}", yes(fs::read("/data/note.txt").map_or(false, |b| b == note.as_bytes())));
        return 0;
    }
    // quota: create files until the store says no
    let mut made = 0;
    for i in 0..200 {
        if fs::write(&format!("/data/q{i}"), b"x").is_ok() {
            made += 1;
        }
    }
    println!("[homeprobe {tag}] files made until the quota stopped it: {made}");
    for i in 0..200 {
        let _ = fs::remove_file(&format!("/data/q{i}"));
    }
    // a child: confined to the same home
    let mut cmd = Command::new("homeprobe");
    cmd.arg(&user).arg("child").files(true);
    match cmd.spawn() {
        Ok(child) => {
            let st = child.wait();
            let theirs = fs::read("/data/child.txt").map_or(false, |b| b == format!("child of {user}").as_bytes());
            println!("[homeprobe {tag}] child ended ({}), its file is in my home: {}", yes(st.is_ok()), yes(theirs));
        }
        Err(_) => println!("[homeprobe {tag}] child could not be started"),
    }
    println!("[homeprobe {tag}] done");
    // stay alive until logged out
    loop {
        sleep(10);
    }
});
