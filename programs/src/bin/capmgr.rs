#![no_std]
#![no_main]
//! The capabilities manager (`../../docs/DESKTOP.md` §11.3): `metal-capmgr` on the machine. Permissions from the program manager
//! (INFO, STATE, POLICY_GET / POLICY_SET, GRANT); capabilities from the kernel (`SYSINFO` 10 to 14); revoking and restoring with
//! `REVOKE_OF` / `RESTORE_OF` and the revoke authority the manager gives programs with `permission = admin` (slot 4).
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use metal_capmgr::{CapManager, CapSystem, DerivNode, Holder, Live, ObjectInfo, PermState, Preview, ProgramInfo, Revoked, Slot, Trust};
use metal_compositor_os::OsTransport;
use metal_desktop_proto::client::Connection;
use metal_programs::desk;
use metal_programs::*;
use metal_ui::MemPlatform;

const MAX_TASKS: u64 = 64;
const REVOKE_OF: u64 = 40;
const RESTORE_OF: u64 = 41;
/// The revoke authority (`Admin` with `R_REVOKE`), from the program manager.
const ADMIN_SLOT: u64 = 4;
/// The slot each capability permission stands for: display 2, shell 3, admin 4.
fn perm_slot(perm: usize) -> Option<u64> {
    match perm {
        2 => Some(2),
        3 => Some(3),
        5 => Some(4),
        _ => None,
    }
}

fn kind_name(k: u64) -> String {
    String::from(desk::KINDS.get(k as usize).copied().unwrap_or("?"))
}

fn task_name(t: u64) -> String {
    desk::task_row(t).map_or_else(|| format!("task {t}"), |r| r.0)
}

fn kernel_error(e: i64) -> String {
    match e {
        -1 => String::from("not allowed: only programs started at run time can be named, never the root capability of an object"),
        -2 => String::from("that slot is empty"),
        -5 => String::from("the kernel refused (invalid)"),
        e => format!("the kernel refused (error {e})"),
    }
}

struct Machine {
    keys: Vec<(String, String)>,
    allow_unsigned: bool,
}

impl CapSystem for Machine {
    fn programs(&mut self) -> Vec<ProgramInfo> {
        ctl::installed()
            .into_iter()
            .map(|(id, name)| {
                let (_, _, _, declared, signed) = desk::info(&id).unwrap_or((0, 0, 0, 0, false));
                let stored = ctl::policy(&id).map_or(0, |p| p.off);
                let st = desk::state(&id);
                let task = st.and_then(|s| s.4);
                // what this start runs with: what was given up at the start (the stored policy of then, and the start's own)
                let started_off = st.map_or(0, |s| s.3);
                let perms = core::array::from_fn(|p| {
                    let bit = 1u8 << p;
                    let declared_p = declared & bit != 0;
                    let live = match task {
                        None => Live::NotRunning,
                        Some(t) => match perm_slot(p) {
                            // a capability permission: is the capability in its slot now?
                            Some(slot) => {
                                // (the slot holds what the permission stands for: an endpoint for display and shell, the revoke
                                // authority for admin; anything else there is not it)
                                let kind = if p == 5 { 6 } else { 1 };
                                if declared_p && desk::cap_slot(t, slot).is_some_and(|c| c.0 == kind) {
                                    Live::On
                                } else {
                                    Live::Off
                                }
                            }
                            // a session permission: what it started with, until it starts again
                            None => {
                                let had = declared_p && started_off & bit == 0;
                                let will = declared_p && stored & bit == 0;
                                match (had, will) {
                                    (true, true) => Live::On,
                                    (false, false) => Live::Off,
                                    (true, false) => Live::OnUntilRestart,
                                    (false, true) => Live::OffUntilRestart,
                                }
                            }
                        },
                    };
                    PermState { declared: declared_p, stored_off: stored & bit != 0, start_off: started_off & bit != 0 && stored & bit == 0, live }
                });
                ProgramInfo { id, name, task: task.map(|t| t as u32), signed_by: signed.then(|| self.keys.first().map_or_else(|| String::from("a trusted key"), |k| k.0.clone())), perms }
            })
            .collect()
    }

    fn set_policy(&mut self, id: &str, perm: usize, off: bool) -> Result<(), String> {
        let p = ctl::policy(id).ok_or("not installed")?;
        let bit = 1u8 << perm;
        let new = if off { p.off | bit } else { p.off & !bit };
        if ctl::set_policy(id, new) {
            Ok(())
        } else {
            Err(String::from("the program manager refused"))
        }
    }

    fn take_now(&mut self, id: &str, perm: usize, keep_off: bool) -> Result<(), String> {
        let slot = perm_slot(perm).ok_or("this permission is a session: it changes at the next start")?;
        let task = desk::state(id).and_then(|s| s.4).ok_or("it is not running")?;
        let r = svc(REVOKE_OF, ADMIN_SLOT, task, slot, 0, 0)[0] as i64;
        if r < 0 {
            return Err(kernel_error(r));
        }
        if keep_off {
            self.set_policy(id, perm, true)?;
        }
        Ok(())
    }

    fn give_now(&mut self, id: &str, perm: usize) -> Result<(), String> {
        desk::grant(id, 1 << perm).map_err(|e| match e {
            4 => String::from("refused: the manifest does not declare it, or the stored policy keeps it off"),
            3 => String::from("it is not running"),
            _ => format!("the program manager refused ({e})"),
        })
    }

    fn tasks(&mut self) -> Vec<(u32, String)> {
        (0..MAX_TASKS).filter_map(|t| desk::task_row(t).filter(|r| r.1 != 7).map(|r| (t as u32, r.0))).collect()
    }

    fn slots(&mut self, task: u32) -> Vec<Slot> {
        (0..32u64)
            .filter_map(|s| {
                let (kind, obj, rights, node) = desk::cap_slot(task as u64, s)?;
                if kind == 0 {
                    return None;
                }
                let object_name = desk::object(kind, obj).map(|o| o.0).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("#{obj}"));
                Some(Slot { slot: s as u8, kind: kind_name(kind), object: obj as u32, object_name, rights: desk::rights_text(kind, rights), node: node as u32 })
            })
            .collect()
    }

    fn preview(&mut self, task: u32, slot: u8) -> Preview {
        match desk::sysinfo(13, task as u64 | (slot as u64) << 8) {
            Some(r) => Preview {
                derived: r[1] as u32,
                tasks: (0..MAX_TASKS as u32).filter(|t| r[2] & 1 << t != 0).collect(),
                refused: (r[4] == 1).then(|| String::from("the root capability of its object: revoking it would destroy the object")),
            },
            None => Preview { derived: 0, tasks: Vec::new(), refused: Some(String::from("the kernel does not say")) },
        }
    }

    fn revoke(&mut self, task: u32, slot: u8) -> Result<u32, String> {
        let r = svc(REVOKE_OF, ADMIN_SLOT, task as u64, slot as u64, 0, 0)[0] as i64;
        if r < 0 {
            Err(kernel_error(r))
        } else {
            Ok(r as u32)
        }
    }

    fn revoked(&mut self, task: u32) -> Vec<Revoked> {
        (0..16u64)
            .map_while(|n| desk::sysinfo(14, task as u64 | n << 8))
            .map(|r| {
                let (kind, obj) = (r[3], r[4]);
                Revoked {
                    id: r[1] as u32,
                    slot: r[2] as u8,
                    kind: kind_name(kind),
                    object_name: desk::object(kind, obj).map(|o| o.0).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("#{obj}")),
                    rights: desk::rights_text(kind, r[5] & 0xFFFF),
                    restorable: r[5] & 1 << 16 != 0,
                }
            })
            .collect()
    }

    fn restore(&mut self, task: u32, id: u32) -> Result<(), String> {
        let r = svc(RESTORE_OF, ADMIN_SLOT, task as u64, id as u64, 0, 0)[0] as i64;
        if r < 0 {
            Err(kernel_error(r))
        } else {
            Ok(())
        }
    }

    fn object(&mut self, task: u32, slot: u8) -> Option<ObjectInfo> {
        let (kind, obj, _, _) = desk::cap_slot(task as u64, slot as u64)?;
        if kind == 0 {
            return None;
        }
        let (name, _, _, creator) = desk::object(kind, obj)?;
        let holders_mask = desk::sysinfo(11, kind | obj << 8).map_or(0, |r| r[1]);
        let mut holders = Vec::new();
        for t in (0..MAX_TASKS).filter(|t| holders_mask & 1 << t != 0) {
            for s in 0..32u64 {
                if let Some((k, o, rights, _)) = desk::cap_slot(t, s) {
                    if k == kind && o == obj {
                        holders.push(Holder { task: t as u32, name: task_name(t), slot: s as u8, rights: desk::rights_text(k, rights), creator: creator == Some(t) });
                    }
                }
            }
        }
        Some(ObjectInfo { id: obj as u32, kind: kind_name(kind), name: if name.is_empty() { format!("#{obj}") } else { name }, creator: creator.map(|c| (c as u32, task_name(c))), holders })
    }

    fn tree(&mut self, _task: u32, _slot: u8) -> Option<DerivNode> {
        // (the kernel does not report the derivation tree itself, only who holds what: SYSINFO 11)
        None
    }

    fn trust(&mut self) -> Trust {
        Trust { keys: self.keys.clone(), allow_unsigned: self.allow_unsigned }
    }
}

fn main() -> u64 {
    let keys: Vec<(String, String)> = fs::read_to_string("/system/etc/program-keys")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    let allow_unsigned = fs::read_to_string("/system/etc/program-policy").unwrap_or_default().lines().any(|l| l.replace(' ', "") == "allow_unsigned=true");
    let mut t = OsTransport::new(2);
    let mut conn = match Connection::hello(&mut t) {
        Ok(c) => c,
        Err(e) => {
            println!("[capmgr] no display: error {e}");
            return 1;
        }
    };
    let mut buf = [0u8; 256];
    let theme = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])).unwrap_or(metal_ui::theme::LIGHT);
    let mut app = match CapManager::open(&mut conn, &mut t, Box::new(Machine { keys, allow_unsigned }), &theme) {
        Ok(a) => a,
        Err(e) => {
            println!("[capmgr] cannot open its window: error {e}");
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

program!("capmgr", |_c| main());
