#![no_std]
#![no_main]
extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use metal_programs::*;

// Test driver and command-line client for the program manager. Slot 0 is a send capability to the control endpoint.
// Runs the program-manager scenario and prints one TEST line per check; the exit code is the number of failures.
const OP_INSTALL: u64 = 1;
const OP_UNINSTALL: u64 = 2;
const OP_START: u64 = 3;
const OP_STOP: u64 = 4;
const OP_STATE: u64 = 5;
const OP_FOREGROUND: u64 = 6;
const OP_BACKGROUND: u64 = 7;
const OP_LIST: u64 = 8;
const OP_INFO: u64 = 10;
const OP_SAVED: u64 = 11;

fn pack(id: &str) -> (u64, u64) {
    let mut b = [0u8; 16];
    let n = id.len().min(16);
    b[..n].copy_from_slice(&id.as_bytes()[..n]);
    (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
}

fn ask(op: u64, ab: u64, id: &str) -> [u64; 3] {
    let (a, b) = pack(id);
    match call(0, [op | ab << 8, a, b]) {
        (0, r) => r,
        (e, _) => [1000 + (-e) as u64, 0, 0],
    }
}

struct State {
    state: u64,
    restarts: u64,
    running: bool,
    exit: u64,
}

fn state(id: &str, ab: u64) -> State {
    let r = ask(OP_STATE, ab, id);
    State { state: r[1] & 0xFF, restarts: (r[1] >> 8) & 0xFFFF, running: (r[1] >> 24) & 1 == 1, exit: r[2] }
}

fn saved(id: &str, ab: u64) -> (u64, u64) {
    let r = ask(OP_SAVED, ab, id);
    (r[1], r[2])
}

/// Poll `f` for up to `ticks` ticks.
fn until(ticks: u64, mut f: impl FnMut() -> bool) -> bool {
    let t0 = metal_programs::ticks();
    loop {
        if f() {
            return true;
        }
        if metal_programs::ticks() - t0 > ticks {
            return false;
        }
        sleep(2);
    }
}

fn state_name(s: u64) -> &'static str {
    match s {
        0 => "stopped",
        1 => "running (service)",
        2 => "foreground",
        3 => "background",
        4 => "stopping",
        5 => "failed (crashed too often)",
        _ => "?",
    }
}

const FG: u64 = 2;
const BG: u64 = 3;

program!("progctl", |c| {
    // manual use (from the shell: `program ...`): progctl install|uninstall|start|stop|foreground|background|state|info|list|available ID
    if let Some(cmd) = c.argv(1) {
        let id = c.argv(2).unwrap_or("");
        let need_id = |id: &str| -> bool {
            if id.is_empty() {
                println!("usage: prog {} <id>   (try: prog available, prog list)", c.argv(1).unwrap_or(""));
            }
            !id.is_empty()
        };
        let say = |what: &str, st: u64| -> u64 {
            let why = match st {
                0 => "ok",
                1 => "no such program (not installed, or no such package)",
                2 => "no such ability (or the package is invalid)",
                3 => "wrong state (already running, not running, or the id differs from the file name)",
                4 => "refused",
                5 => "failed",
                6 => "refused: the package is not signed and unsigned programs are not allowed",
                7 => "no room: 16 abilities have been started already",
                8 => "refused: signed by a key this system does not trust",
                9 => "refused: the signature does not match the package",
                _ => "error",
            };
            println!("{what}: {why}");
            st
        };
        return match cmd {
            "list" => {
                let mut i = 0;
                let mut n = 0;
                while let r @ [0, ..] = call(0, [OP_LIST, i, 0]).1 {
                    let mut b = [0u8; 16];
                    b[..8].copy_from_slice(&r[1].to_le_bytes());
                    b[8..].copy_from_slice(&r[2].to_le_bytes());
                    let name = core::str::from_utf8(&b).unwrap_or("?").trim_end_matches('\0');
                    let st = state(name, 0);
                    println!("{name:<16} {}", if st.running || st.state != 0 { state_name(st.state) } else { "installed" });
                    i += 1;
                    n += 1;
                }
                if n == 0 {
                    println!("no programs installed (see: prog available)");
                }
                0
            }
            "available" => {
                match fs::read_dir("/system/usr/programs") {
                    Ok(v) => {
                        for e in v {
                            if let Some(n) = e.name.strip_suffix(".map") {
                                println!("{n}");
                            }
                        }
                        0
                    }
                    Err(_) => {
                        println!("cannot read /system/usr/programs");
                        1
                    }
                }
            }
            "install" if need_id(id) => say("install", ask(OP_INSTALL, 0, id)[0]),
            "uninstall" | "remove" if need_id(id) => say("uninstall", ask(OP_UNINSTALL, 0, id)[0]),
            "start" if need_id(id) => {
                let r = ask(OP_START, c.argv(3).and_then(|a| a.parse().ok()).unwrap_or(0), id);
                if r[0] == 0 {
                    println!("started {id} (board entry {}, task {})", r[1], r[2]);
                    0
                } else {
                    say("start", r[0])
                }
            }
            "stop" if need_id(id) => say("stop", ask(OP_STOP, 0, id)[0]),
            "foreground" | "fg" if need_id(id) => say("foreground", ask(OP_FOREGROUND, 0, id)[0]),
            "background" | "bg" if need_id(id) => say("background", ask(OP_BACKGROUND, 0, id)[0]),
            "state" if need_id(id) => {
                let r = ask(OP_STATE, 0, id);
                if r[0] != 0 {
                    return say("state", r[0]);
                }
                let st = state(id, 0);
                let sv = saved(id, 0);
                println!("{id}: {}, {} restarts, {}, last exit {}", state_name(st.state), st.restarts, if st.running { "running" } else { "not running" }, if st.exit == u64::MAX { alloc::string::String::from("none") } else { alloc::format!("{:#x}", st.exit) });
                println!("saved words: {} {:#x}", sv.0, sv.1);
                0
            }
            "info" if need_id(id) => {
                let r = ask(OP_INFO, 0, id);
                if r[0] != 0 {
                    return say("info", r[0]);
                }
                let class = ["interactive", "background", "heavy"].get(((r[1] >> 20) & 15) as usize).copied().unwrap_or("?");
                println!("{id}: {} heap pages, {} threads, class {class}, files {}, network {}, {} abilities, {}", r[1] & 0xFFFF, (r[1] >> 16) & 15, (r[1] >> 24) & 1, (r[1] >> 25) & 1, r[2], if (r[1] >> 27) & 1 == 1 { "signed" } else { "unsigned" });
                if (r[1] >> 32) & 1 == 1 {
                    println!("{id}: starts programs (permission spawn), may hand them {} pages", r[1] >> 33);
                }
                0
            }
            _ => {
                println!("usage: prog list | available | install ID | uninstall ID (or remove ID)");
                println!("       prog start ID | stop ID | foreground ID | background ID | state ID | info ID");
                2
            }
        };
    }

    // The data partition outlives a boot (and `make test` boots several times on one disk image): start from nothing, and
    // leave nothing (the programs are uninstalled at the end, which removes their directories).
    let ids = ["counter", "crasher", "hog", "probefs", "probenofs", "probespawn", "probespawnall", "fsa", "fsb"];
    for id in ids {
        ask(OP_STOP, 0, id);
    }
    until(300, || ids.iter().all(|id| !state(id, 0).running));
    for id in ids {
        ask(OP_UNINSTALL, 0, id);
        // (data left by a run that did not get to uninstall: the manager only cleans up programs it knows)
        if let Ok(list) = fs::read_dir(&alloc::format!("/data/programdata/{id}")) {
            for e in list {
                let _ = fs::remove_file(&alloc::format!("/data/programdata/{id}/{}", e.name));
            }
            let _ = fs::remove_dir(&alloc::format!("/data/programdata/{id}"));
        }
    }

    let mut failed = 0u64;
    let mut check = |name: &str, ok: bool, detail: &str| {
        // (not println!: that cuts a line at 200 bytes)
        write_out(alloc::format!("[progctl] TEST {name}{}{detail}: {}\n", if detail.is_empty() { "" } else { " " }, if ok { "PASS" } else { "FAIL" }).as_bytes());
        if !ok {
            failed += 1;
        }
    };

    // ---- packages: installing ----
    let listed = |id: &str| {
        let mut i = 0;
        while let r @ [0, ..] = call(0, [OP_LIST, i, 0]).1 {
            if r[1..] == pack(id).into_tuple() {
                return true;
            }
            i += 1;
        }
        false
    };
    let mut codes: Vec<u64> = Vec::new();
    for id in ids {
        codes.push(ask(OP_INSTALL, 0, id)[0]);
    }
    check("program packages in the system image install", codes.iter().all(|&c| c == 0) && listed("counter") && listed("probenofs"), &alloc::format!("(statuses {codes:?})"));
    let bad = ask(OP_INSTALL, 0, "corrupt")[0];
    let wrong = ask(OP_INSTALL, 0, "mismatch")[0];
    let unsigned = ask(OP_INSTALL, 0, "nosign")[0];
    let missing = ask(OP_INSTALL, 0, "nothere")[0];
    check("a corrupt package, one whose id differs from its file name, and a missing one are refused", bad == 2 && wrong == 3 && missing == 1 && !listed("corrupt") && !listed("mismatch"), &alloc::format!("({bad} {wrong} {missing})"));
    let rogue = ask(OP_INSTALL, 0, "rogue")[0];
    let tampered = ask(OP_INSTALL, 0, "tampered")[0];
    check("signatures: an unsigned package, one signed by a key the system does not trust, and a signed one that was changed are refused", unsigned == 6 && rogue == 8 && tampered == 9 && !listed("nosign") && !listed("rogue") && !listed("tampered"), &alloc::format!("(unsigned {unsigned}, untrusted {rogue}, tampered {tampered})"));
    let info = ask(OP_INFO, 0, "probefs");
    let info2 = ask(OP_INFO, 0, "probenofs");
    check("the manifest's limits and permissions, and that it was signed, are what the manager reports", info[0] == 0 && info[1] & 0xFFFF == 48 && (info[1] >> 24) & 1 == 1 && (info2[1] >> 24) & 1 == 0 && (info[1] >> 27) & 1 == 1 && info[2] == 1, &alloc::format!("({:#x} {:#x})", info[1], info2[1]));

    // ---- lifecycle ----
    let r = ask(OP_START, 0, "counter");
    let task_a = r[2];
    check("a program starts and its UI ability comes to the foreground", r[0] == 0 && until(100, || state("counter", 0).state == FG), "");
    check("on_create and on_foreground ran", until(100, || saved("counter", 0).1 & 3 == 3), "");
    let n0 = saved("counter", 0).0;
    check("starting it again is refused", ask(OP_START, 0, "counter")[0] == 3, "");
    check("the manager moves it to the background and the ability is told (on_background)", ask(OP_BACKGROUND, 0, "counter")[0] == 0 && until(100, || saved("counter", 0).1 & 4 == 4) && state("counter", 0).state == BG, "");
    check("... and back to the foreground", ask(OP_FOREGROUND, 0, "counter")[0] == 0 && until(100, || state("counter", 0).state == FG), "");
    check("the ability's own work (on_tick) runs between lifecycle changes", until(200, || saved("counter", 0).0 > n0 + 3), &alloc::format!("(count {} -> {})", n0, saved("counter", 0).0));

    // ---- isolation: a crash is one program's problem ----
    let cr = ask(OP_START, 0, "crasher");
    let crashed = until(300, || state("crasher", 0).restarts >= 1);
    check("a crashing program is restarted by the manager", cr[0] == 0 && crashed, &alloc::format!("(restarts {})", state("crasher", 0).restarts));
    let (n_before, _) = saved("crasher", 0);
    let restored = until(300, || {
        let (n, tag) = saved("crasher", 0);
        tag == 2 && n > n_before.max(9)
    });
    check("the restarted instance got what the crashed one had saved and carries on from it", restored, &alloc::format!("(saved {:?})", saved("crasher", 0)));
    let st = state("counter", 0);
    check("the other program did not notice: same task, still in the foreground, still counting", st.running && st.restarts == 0 && st.state == FG && saved("counter", 0).0 > n0, &alloc::format!("(task {task_a}, restarts {})", st.restarts));
    check("a crash does not stop the crasher for good: it is running and not marked failed", state("crasher", 0).running && state("crasher", 0).state != 5, "");

    // ---- limits: memory ----
    let hg = ask(OP_START, 0, "hog");
    let failed_state = until(1500, || state("hog", 0).state == 5);
    let hs = state("hog", 0);
    check("a program that runs into its memory limit dies alone; after repeated crashes the manager gives up on it", hg[0] == 0 && failed_state && !hs.running && hs.restarts >= 3, &alloc::format!("(restarts {}, last exit {:#x})", hs.restarts, hs.exit));
    check("... while the others keep running", state("counter", 0).running && state("crasher", 0).running, "");

    // ---- permissions ----
    let pf = ask(OP_START, 0, "probefs");
    let pn = ask(OP_START, 0, "probenofs");
    let ended = until(200, || saved("probefs", 0).1 == 0xFEED && saved("probenofs", 0).1 == 0xFEED);
    let (ef, en) = (saved("probefs", 0).0, saved("probenofs", 0).0);
    check("a program sees files only with the files permission; no program can use the manager's control endpoint or start programs", pf[0] == 0 && pn[0] == 0 && ended && ef == 15 && en == 14, &alloc::format!("(answers {ef} and {en}: bit 0 file, bit 1 control refused, bit 2 no spawn, bit 3 no spawn with files)"));
    let ps = ask(OP_START, 0, "probespawn");
    let pa = ask(OP_START, 0, "probespawnall");
    let ended = until(200, || saved("probespawn", 0).1 == 0xFEED && saved("probespawnall", 0).1 == 0xFEED);
    let (es, ea) = (saved("probespawn", 0).0, saved("probespawnall", 0).0);
    let si = ask(OP_INFO, 0, "probespawn");
    check("a program starts programs only with the spawn permission, and its children get file access only if it has allfiles", ps[0] == 0 && pa[0] == 0 && ended && es == 11 && ea == 3 && (si[1] >> 32) & 1 == 1 && si[1] >> 33 == 64, &alloc::format!("(answers {es} of 11 and {ea} of 3, info {:#x})", si[1]));

    // ---- storage: for a program, /data is its own directory ----
    let sa = ask(OP_START, 0, "fsa");
    let sb = ask(OP_START, 0, "fsb");
    let answered = until(300, || saved("fsa", 0).1 == 0xFEED && saved("fsb", 0).1 == 0xFEED);
    let (aa, ab) = (saved("fsa", 0).0, saved("fsb", 0).0);
    check("a program's storage is private: /data is its own directory, nothing outside it can be named, and it has a quota", sa[0] == 0 && sb[0] == 0 && answered && aa == 0x1FF && ab == 0x1FF, &alloc::format!("(answers {aa:#x} and {ab:#x} of 0x1ff)"));
    let (na, nb) = (fs::read("/data/programdata/fsa/note.txt").unwrap_or_default(), fs::read("/data/programdata/fsb/note.txt").unwrap_or_default());
    check("each program's files are where the manager put them and hold only that program's data", na == b"secret-of-fsa" && nb == b"secret-of-fsb" && fs::read("/data/programdata/fsa/f0").is_ok(), "");
    let fsb_files = fs::read_dir("/data/programdata/fsb").map(|l| l.len()).unwrap_or(0);
    check("... and a program that makes files until it is stopped holds at most 12 of the store's entries", (9..=13).contains(&fsb_files), &alloc::format!("({fsb_files} files)"));
    check("uninstalling a program removes its data", ask(OP_UNINSTALL, 0, "fsa")[0] == 0 && fs::read_dir("/data/programdata/fsa").is_err() && fs::read("/data/programdata/fsb/note.txt").is_ok(), "");

    // ---- stopping ----
    let st0 = state("counter", 0);
    check("stopping asks the ability to leave and it does (on_destroy ran, exit code 0)", ask(OP_STOP, 0, "counter")[0] == 0 && until(300, || !state("counter", 0).running) && saved("counter", 0).1 & 8 == 8 && state("counter", 0).exit == 0 && state("counter", 0).state == 0, &alloc::format!("(was task {task_a}, running {})", st0.running));
    check("stopping what is not running is refused; uninstalling a running program is refused", ask(OP_STOP, 0, "counter")[0] == 3 && ask(OP_UNINSTALL, 0, "crasher")[0] == 4, "");
    ask(OP_STOP, 0, "crasher");
    until(300, || !state("crasher", 0).running);
    check("a program that is not running can be uninstalled, and is gone from the list", ask(OP_UNINSTALL, 0, "crasher")[0] == 0 && !listed("crasher") && ask(OP_START, 0, "crasher")[0] == 1, "");
    check("a program can be installed again after being uninstalled, and starts fresh", ask(OP_INSTALL, 0, "crasher")[0] == 0 && ask(OP_START, 0, "crasher")[0] == 0, "");
    ask(OP_STOP, 0, "crasher");
    until(300, || !state("crasher", 0).running);
    // leave the disk as found: the programs and their data go
    for id in ids {
        ask(OP_STOP, 0, id);
    }
    until(300, || ids.iter().all(|id| !state(id, 0).running));
    for id in ids {
        ask(OP_UNINSTALL, 0, id);
    }
    let _ = String::new();
    failed
});

trait IntoTuple {
    fn into_tuple(self) -> [u64; 2];
}
impl IntoTuple for (u64, u64) {
    fn into_tuple(self) -> [u64; 2] {
        [self.0, self.1]
    }
}
