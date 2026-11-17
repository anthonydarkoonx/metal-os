#![no_std]
#![no_main]
//! The desktop (`../../docs/DESKTOP.md` §10): `metal-desktop` on the machine. The program manager gives it the compositor's
//! desktop role when it starts it (P5). Its `Env` is the system: the clock, the program manager, the files of its own folder.
extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use metal_compositor_os::OsTransport;
use metal_desktop::{Config, Desktop, Env, MemInfo, ProgramEntry, SecurityInfo};
use metal_programs::desk;
use metal_programs::*;
use metal_ui::icons::Icon;

struct Machine;

fn icon_of(id: &str) -> Icon {
    match id {
        "files" => Icon::Folder,
        "settings" => Icon::Gear,
        "desktop" => Icon::Computer,
        "taskmgr" => Icon::TaskManager,
        "terminal" => Icon::Terminal,
        "netconf" => Icon::Network,
        _ => Icon::Program,
    }
}

impl Env for Machine {
    fn now_ms(&self) -> u64 {
        desk::now_ms()
    }
    fn clock(&self) -> Option<(u8, u8)> {
        desk::unix_seconds().map(|s| {
            let c = desk::civil(s);
            (c.3, c.4)
        })
    }
    fn seconds(&self) -> u8 {
        desk::unix_seconds().map_or(0, |s| (s % 60) as u8)
    }
    fn date(&self) -> Option<(u16, u8, u8)> {
        desk::unix_seconds().map(|s| {
            let c = desk::civil(s);
            (c.0, c.1, c.2)
        })
    }
    fn uptime_ms(&self) -> u64 {
        desk::now_ms()
    }
    fn programs(&mut self) -> Vec<ProgramEntry> {
        // (the desktop itself is not something to start)
        ctl::installed().into_iter().filter(|(id, _)| id != "desktop").map(|(id, name)| ProgramEntry { icon: icon_of(&id), id, name }).collect()
    }
    fn start(&mut self, id: &str) -> Result<(), String> {
        if !ctl::installed().iter().any(|(i, _)| i == id) {
            return Err(String::from("it is not installed on this system"));
        }
        if ctl::start(id) {
            Ok(())
        } else {
            Err(String::from("the program manager could not start it (all 16 of its places for running programs may be taken)"))
        }
    }
    fn run(&mut self, command: &str) -> Result<(), String> {
        let id = command.split_whitespace().next().unwrap_or("");
        self.start(id).map_err(|e| alloc::format!("{e}; commands need the terminal program, which is not in this build"))
    }
    fn memory(&mut self) -> Option<MemInfo> {
        let s = svc(SYSINFO, 0, 0, 0, 0, 0);
        let (total, free) = (s[2] * 4, s[3] * 4);
        Some(MemInfo { total_kib: total, used_kib: total.saturating_sub(free), compressed_kib: 0, swapped_kib: 0 })
    }
    fn security(&mut self) -> Option<SecurityInfo> {
        let layout = svc(SYSINFO, 9, 0, 0, 0, 0);
        let policy = fs::read_to_string("/system/etc/program-policy").unwrap_or_default();
        Some(SecurityInfo { developer_mode: policy.lines().any(|l| l.replace(' ', "") == "allow_unsigned=true"), strong_random: layout[4] != 0, aslr: layout[3] != 0 })
    }
    fn load_settings(&mut self) -> Option<String> {
        fs::read_to_string("/data/desktop.conf").ok()
    }
    fn save_settings(&mut self, text: &str) {
        let _ = fs::write("/data/desktop.conf", text.as_bytes());
    }
    fn trusts_settings_from(&self, program: u32) -> bool {
        // (any instance of the settings program)
        desk::tasks_of("settings").into_iter().any(|t| desk::program_of(t) == program as u64)
    }
    fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }
}

fn main() -> u64 {
    let mut t = OsTransport::new(2);
    let mut env = Machine;
    let config = Config { pinned: vec![String::from("files"), String::from("settings")] };
    let mut d = match Desktop::start(&mut t, &mut env, &config, &metal_ui::theme::LIGHT) {
        Ok(d) => d,
        Err(e) => {
            println!("[desktop] cannot start: error {e}");
            return 1;
        }
    };
    println!("[desktop] up");
    loop {
        let seen = d.conn.event_word(&mut t);
        d.pump(&mut t, &mut env);
        for l in d.log.drain(..) {
            println!("[desktop] {l}");
        }
        // (the clock and the applets want a look at least once a second)
        d.conn.wait(&mut t, seen, 500);
    }
}

program!("desktop", |_c| main());
