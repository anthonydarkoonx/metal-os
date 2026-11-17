#![no_std]
#![no_main]
//! The settings program (`../../docs/DESKTOP.md` §11.4): `metal-settings` on the machine. Its settings go to the desktop through
//! the compositor (SETTINGS_SEND); its own copy is `/data/settings.conf` (its folder: `/data/programdata/settings`).
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use metal_compositor_os::OsTransport;
use metal_desktop_proto::client::Connection;
use metal_programs::desk;
use metal_programs::*;
use metal_settings::{About, Settings, System};
use metal_ui::MemPlatform;

struct Machine;

impl System for Machine {
    fn load(&mut self) -> Option<String> {
        fs::read_to_string("/data/settings.conf").ok()
    }
    fn save(&mut self, text: &str) -> Result<(), String> {
        fs::write("/data/settings.conf", text.as_bytes()).map_err(|_| String::from("the data store refused the file"))
    }
    fn programs(&mut self) -> Vec<(String, String)> {
        ctl::installed().into_iter().filter(|(id, _)| id != "desktop").collect()
    }
    fn wallpapers(&mut self) -> Vec<String> {
        let dir = "/system/usr/share/wallpapers";
        let mut v: Vec<String> = fs::read_dir(dir).map(|l| l.into_iter().filter(|e| !e.is_dir && e.name.ends_with(".ppm")).map(|e| format!("{dir}/{}", e.name)).collect()).unwrap_or_default();
        v.sort();
        v
    }
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }
    fn openers(&mut self) -> Vec<(String, Option<String>)> {
        // (which program opens which kind of file is the manifests' `opens` key: proposal P4, not in this program manager)
        Vec::new()
    }
    fn set_opener(&mut self, _ext: &str, _program: Option<&str>) -> Result<(), String> {
        Err(String::from("the program manager has no file kinds (proposal P4)"))
    }
    fn about(&mut self) -> About {
        let s = svc(SYSINFO, 0, 0, 0, 0, 0);
        let layout = svc(SYSINFO, 9, 0, 0, 0, 0);
        let release = fs::read_to_string("/system/etc/os-release").unwrap_or_default();
        let field = |k: &str| release.lines().find_map(|l| l.strip_prefix(k).map(String::from));
        let version = format!("{} {} (AArch64)", field("NAME=").unwrap_or_else(|| String::from("Metal OS")), field("VERSION=").unwrap_or_default());
        let policy = fs::read_to_string("/system/etc/program-policy").unwrap_or_default();
        About {
            version,
            cores: s[4] as u32,
            memory_mib: (s[2] * 4096 >> 20) as u32,
            entropy: String::from(if layout[4] != 0 { "the processor's random number instructions (strong)" } else { "timer jitter (weak)" }),
            kaslr: layout[3] != 0,
            aslr: layout[3] != 0,
            developer_mode: policy.lines().any(|l| l.replace(' ', "") == "allow_unsigned=true"),
        }
    }
    fn start(&mut self, id: &str) -> Result<(), String> {
        if !ctl::installed().iter().any(|(i, _)| i == id) {
            return Err(String::from("it is not installed on this system"));
        }
        if ctl::start(id) {
            Ok(())
        } else {
            Err(String::from("the program manager could not start it"))
        }
    }
}

fn main() -> u64 {
    let mut t = OsTransport::new(2);
    let mut conn = match Connection::hello(&mut t) {
        Ok(c) => c,
        Err(e) => {
            println!("[settings] no display: error {e}");
            return 1;
        }
    };
    let mut buf = [0u8; 256];
    let theme = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])).unwrap_or(metal_ui::theme::LIGHT);
    let mut app = match Settings::open(&mut conn, &mut t, Box::new(Machine), &theme, 0) {
        Ok(a) => a,
        Err(e) => {
            println!("[settings] cannot open its window: error {e}");
            return 1;
        }
    };
    let mut p = MemPlatform::default();
    loop {
        let seen = conn.event_word(&mut t);
        p.now = desk::now_ms();
        while let Some(e) = conn.next_event(&mut t) {
            app.handle(&mut conn, &mut t, &e, &mut p);
        }
        app.update(&mut conn, &mut t);
        if app.win.close_requested {
            conn.bye(&mut t);
            return 0;
        }
        conn.wait(&mut t, seen, 1000);
    }
}

program!("settings", |_c| main());
