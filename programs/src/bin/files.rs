#![no_std]
#![no_main]
//! The file manager (`../../docs/DESKTOP.md` §11.2): `metal-files` on the machine, over the VFS (with `permission = allfiles`: the
//! whole disk) and the program manager.
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use metal_compositor_os::OsTransport;
use metal_desktop_proto::client::Connection;
use metal_files::fsops::{join, Entry, Fs, FsError, StoreInfo};
use metal_files::{Env, FileManager};
use metal_programs::desk;
use metal_programs::*;
use metal_ui::MemPlatform;

/// The VFS.
struct Vfs(fs::Fs);

fn err(e: i64) -> FsError {
    match e {
        1 => FsError::NotFound,
        2 => FsError::IsDir,
        3 => FsError::BadHandle,
        4 => FsError::ReadOnly,
        5 => FsError::NoSpace,
        6 => FsError::Invalid,
        8 => FsError::Exists,
        9 => FsError::NotEmpty,
        e if e < 0 => FsError::Permission,
        _ => FsError::Io,
    }
}

impl Fs for Vfs {
    fn list(&mut self, dir: &str) -> Result<Vec<Entry>, FsError> {
        let mut buf = alloc::vec![0u8; 4096];
        let n = self.0.list(dir, &mut buf).map_err(err)?;
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        Ok(text
            .split('\n')
            .filter(|s| !s.is_empty())
            .map(|s| match s.strip_suffix('/') {
                Some(d) => Entry { name: d.to_string(), dir: true, size: 0 },
                None => {
                    let size = self.0.stat(&join(dir, s)).map_or(0, |(size, _)| size);
                    Entry { name: s.to_string(), dir: false, size }
                }
            })
            .collect())
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        fs::read(path).map_err(|_| match self.0.stat(path) {
            Err(e) => err(e),
            Ok((_, 2)) => FsError::IsDir,
            Ok(_) => FsError::Io,
        })
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.0.write_file(path, data).map_err(err)
    }
    fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
        self.0.mkdir(path).map_err(err)
    }
    fn unlink(&mut self, path: &str) -> Result<(), FsError> {
        self.0.unlink(path).map_err(err)
    }
    fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        self.0.rename(from, to).map_err(err)
    }
    fn has_rename(&self) -> bool {
        // (the VFS renames within /data in one commit: op 63)
        true
    }
    fn store(&mut self) -> StoreInfo {
        // (the VFS does not report the store's space yet)
        StoreInfo::default()
    }
}

struct Machine;

impl Env for Machine {
    fn programs(&mut self) -> Vec<(String, String)> {
        ctl::installed().into_iter().filter(|(id, _)| id != "desktop").collect()
    }
    fn opens(&mut self, _ext: &str) -> Option<String> {
        // (no program declares the kinds of file it opens yet: proposal P4)
        None
    }
    fn open_with(&mut self, program: &str, path: &str) -> Result<(), String> {
        // START has no argument yet (proposal P3): the program starts, without the file
        if ctl::start(program) {
            Err(format!("{program} was started, but without {path}: starting a program with a file needs proposal P3"))
        } else {
            Err(format!("the program manager could not start {program}"))
        }
    }
    fn install(&mut self, path: &str, _data: &[u8]) -> Result<String, String> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let id = name.strip_suffix(".map").unwrap_or(name);
        if !(path.starts_with("/system/usr/programs/") || path.starts_with("/data/programs/")) {
            return Err(String::from("the program manager installs packages from /system/usr/programs and /data/programs only"));
        }
        desk::install(id).map(|()| String::from("its signature was checked against the keys this system trusts"))
    }
    fn owner_of(&self, path: &str) -> Option<String> {
        let rest = path.strip_prefix("/data/programdata/")?;
        Some(rest.split('/').next()?.to_string())
    }
}

fn main() -> u64 {
    let Ok(vfs) = fs::Fs::open() else {
        println!("[files] no file system");
        return 1;
    };
    let mut t = OsTransport::new(2);
    let mut conn = match Connection::hello(&mut t) {
        Ok(c) => c,
        Err(e) => {
            println!("[files] no display: error {e}");
            return 1;
        }
    };
    let mut buf = [0u8; 256];
    let theme = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])).unwrap_or(metal_ui::theme::LIGHT);
    let mut app = match FileManager::open(&mut conn, &mut t, Box::new(Vfs(vfs)), Box::new(Machine), &theme, "/data") {
        Ok(a) => a,
        Err(e) => {
            println!("[files] cannot open its window: error {e}");
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
        for l in app.log.drain(..) {
            println!("[files] {l}");
        }
        if app.win.close_requested {
            conn.bye(&mut t);
            return 0;
        }
        conn.wait(&mut t, seen, 1000);
    }
}

program!("files", |_c| main());
