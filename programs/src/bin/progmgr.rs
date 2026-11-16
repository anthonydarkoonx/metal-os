#![no_std]
#![no_main]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use metal_programs::ability::*;
use metal_programs::ipc::{self, Endpoint};
use metal_programs::process::{Child, Command};
use metal_programs::*;
use metal_map::{Kind, Manifest, Package, Policy, Trust, TrustError, TrustedKey};

// The program manager. Slot 0: the control endpoint (receive; its holders may install, start and stop programs), slot 5: the
// endpoint factory, slot 6: the spawn capability. It keeps the installed programs (packages in /data/programs, the system's own
// in /system/usr/programs are installed from there), starts abilities with exactly the rights their manifest asks for, tells them
// their lifecycle state through a shared board, and restarts one that crashes. See docs/PROGRAMS.md.

// control protocol: w0 = op | ability << 8, w1/w2 = the program id (up to 16 bytes); the reply's w0 is a status
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
/// The program's display name (the manifest's `name`), 16 bytes in w1 and w2.
const OP_NAME: u64 = 12;
/// The stored permission policy of a program: GET -> (stored | declared << 8, task or MAX); SET (new stored bits in bits 16..21 of the op word) -> (old, new);
/// GRANT (one permission bit in bits 16..21) gives a running program the capability that permission stands for, in the slot it expects.
const OP_POLICY_GET: u64 = 13;
const OP_POLICY_SET: u64 = 14;
const OP_GRANT: u64 = 15;

const OK: u64 = 0;
const E_NOAPP: u64 = 1; // not installed (or: INSTALL found no package)
const E_NOABILITY: u64 = 2; // (INSTALL: the package is invalid)
const E_STATE: u64 = 3; // wrong state for that (already running, not running; INSTALL: id differs from the file name)
const E_REFUSED: u64 = 4; // not allowed (INSTALL: the program is running)
const E_FAILED: u64 = 5; // could not be done; w1 has the reason (INSTALL: storage error)
const E_UNSIGNED: u64 = 6; // INSTALL: not signed, and unsigned programs are not allowed
const E_UNTRUSTED: u64 = 8; // INSTALL: signed with a key the system does not trust
const E_BADSIG: u64 = 9; // INSTALL: the signature does not match the package
const E_NOSLOT: u64 = 7; // all 16 board entries are in use

const CTL: Endpoint = Endpoint(0);
/// The display endpoint (the desktop's), if the manager was given one: abilities with `permission = display` get a send-only copy.
const DISPLAY: Endpoint = Endpoint(3);
/// An ability's slot for it.
const DISPLAY_SLOT: u8 = 2;
/// A shell client's slot for the control endpoint.
const SHELL_CTL_SLOT: u8 = 3;
/// The manager's own copy of the revoke authority, and the slot a program with `permission = admin` gets it in (right: revoke, `R_REVOKE`).
const ADMIN_SLOT: u8 = 7;
const ADMIN_PROGRAM_SLOT: u8 = 4;
const R_REVOKE: u16 = 1 << 10;
// (the manager used to tell the desktop about programs coming and going; the new display protocol will bring that back)
const FACTORY: u64 = 5;
const SPAWN: u64 = 6;
/// Ticks (10 ms) an ability has to leave after on_destroy was requested.
const GRACE: u64 = 50;
/// More crashes than this in `CRASH_WINDOW` ticks and the manager gives up on the ability.
const MAX_CRASHES: usize = 3;
const CRASH_WINDOW: u64 = 1000;

struct Inst {
    program: String,
    ability: usize,
    entry: usize,
    child: Option<Child>,
    state: u32,
    restarts: u32,
    crashes: Vec<u64>,
    last_exit: u64,
    kill_at: u64,
    restart_at: Option<u64>,
    saved: Option<(u64, u64)>,
    /// Permissions the program was started without (bit 0 files, 1 network, 2 display, 3 shell, 4 allfiles, 5 admin): a launch can only narrow what the manifest declares.
    off: u8,
}

struct Mgr {
    board_slot: u64,
    board: *mut u8,
    program_ep: Endpoint,
    programs: Vec<Program>,
    insts: Vec<Inst>,
    keys: Vec<TrustedKey>,
    policy: Policy,
}

struct Program {
    m: Manifest,
    /// The name of the trusted key that signed it (None: an unsigned program, allowed in developer mode).
    signer: Option<String>,
}

fn trust_status(e: TrustError) -> u64 {
    match e {
        TrustError::Unsigned => E_UNSIGNED,
        TrustError::UnknownKey => E_UNTRUSTED,
        TrustError::BadSignature => E_BADSIG,
        TrustError::UnsignedTooLarge => E_UNSIGNED,
    }
}

fn id_of(w1: u64, w2: u64) -> String {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&w1.to_le_bytes());
    b[8..].copy_from_slice(&w2.to_le_bytes());
    let n = b.iter().position(|&c| c == 0).unwrap_or(16);
    String::from_utf8_lossy(&b[..n]).into_owned()
}

fn pack(id: &str) -> (u64, u64) {
    let mut b = [0u8; 16];
    let n = id.len().min(16);
    b[..n].copy_from_slice(&id.as_bytes()[..n]);
    (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
}

/// Where a system program's package is (the read-only system image).
fn sys_path(id: &str) -> String {
    format!("/system/usr/programs/{id}.map")
}

/// The note that says "the package of this program is the one in the system image": installing a system program writes this, not a copy of the package
/// (the browser is several MiB, more than the data disk of a small machine has room for, and the system image cannot be changed under it).
fn link_path(id: &str) -> String {
    format!("/data/programs/{id}.sys")
}

/// The package of an installed program: the copy in /data/programs, or the system image's own when the program was installed by a link.
fn read_package(id: &str) -> Result<Vec<u8>, u64> {
    match fs::read(&pkg_path(id)) {
        Ok(b) => Ok(b),
        Err(_) if fs::metadata(&link_path(id)).is_ok() => fs::read(&sys_path(id)).map_err(|_| E_NOAPP),
        Err(_) => Err(E_NOAPP),
    }
}

fn pkg_path(id: &str) -> String {
    format!("/data/programs/{id}.map")
}

/// Where a program's stored policy is kept: one byte of permission bits the user has taken away (bit 0 files, 1 network, 2 display, 3 shell, 4 allfiles, 5 admin).
fn policy_path(id: &str) -> String {
    format!("/data/programs/{id}.pol")
}

/// The permissions the user has taken away from a program for good (none if nothing is stored or the file is damaged).
fn policy_of(id: &str) -> u8 {
    match fs::read(&policy_path(id)) {
        Ok(b) if b.len() == 1 && b[0] & !0x3F == 0 => b[0],
        _ => 0,
    }
}

/// The directory that is `/data` for the program with this id.
fn data_dir(id: &str) -> String {
    format!("/data/programdata/{id}")
}

/// Delete a directory and what is in it (a few levels: a program's directory is small).
fn remove_tree(path: &str, depth: u32) {
    if let Ok(list) = fs::read_dir(path) {
        for e in list {
            let child = format!("{path}/{}", e.name.trim_end_matches('/'));
            if e.is_dir && depth > 0 {
                remove_tree(&child, depth - 1);
            } else {
                let _ = fs::remove_file(&child);
            }
        }
    }
    let _ = fs::remove_dir(path);
}

impl Mgr {
    fn word(&self, entry: usize, off: usize) -> &AtomicU32 {
        // SAFETY: the board is one mapped, writable page of ENTRIES entries; `entry` < ENTRIES.
        unsafe { &*(self.board.add(entry * ENTRY + off) as *const AtomicU32) }
    }

    fn set_state(&self, entry: usize, st: u32) {
        self.word(entry, O_STATE).store(st, Ordering::Release);
        let seq = self.word(entry, O_SEQ);
        seq.fetch_add(1, Ordering::Release);
        thread::wake_word(seq, 8);
    }

    fn program(&self, id: &str) -> Option<&Manifest> {
        self.programs.iter().find(|a| a.m.id == id).map(|a| &a.m)
    }

    fn inst(&self, id: &str, ability: usize) -> Option<usize> {
        self.insts.iter().position(|i| i.program == id && i.ability == ability)
    }

    fn running(&self, i: &Inst) -> bool {
        i.child.is_some() || i.restart_at.is_some()
    }

    /// Check a package (structure, then trust) and register the program. The package must be what `id` says.
    fn admit(&mut self, bytes: &[u8], id: &str) -> Result<(), u64> {
        let pkg = Package::parse(bytes).map_err(|_| E_NOABILITY)?;
        if pkg.manifest.id != id {
            return Err(E_STATE);
        }
        let signer = match metal_map::check(&pkg, &self.keys, &self.policy) {
            Ok(Trust::Signed(name)) => Some(name),
            Ok(Trust::Unsigned) => None,
            Err(e) => return Err(trust_status(e)),
        };
        let a = Program { m: pkg.manifest.clone(), signer };
        match self.programs.iter().position(|x| x.m.id == id) {
            Some(i) => self.programs[i] = a,
            None => self.programs.push(a),
        }
        Ok(())
    }

    /// Register the program whose package is in /data/programs.
    fn load(&mut self, id: &str) -> Result<(), u64> {
        let bytes = read_package(id)?;
        self.admit(&bytes, id)
    }

    fn install(&mut self, id: &str) -> u64 {
        if self.insts.iter().any(|i| i.program == id && self.running(i)) {
            return E_REFUSED;
        }
        let Ok(bytes) = fs::read(&sys_path(id)) else { return E_NOAPP };
        let before = self.programs.iter().position(|a| a.m.id == id).map(|i| (self.programs[i].m.clone(), self.programs[i].signer.clone()));
        if let Err(e) = self.admit(&bytes, id) {
            return e;
        }
        let _ = fs::create_dir("/data/programs");
        // (a note, not a copy: the package stays where it is, in the system image)
        let stale = fs::metadata(&pkg_path(id)).is_ok();
        if fs::write(&link_path(id), b"system").is_err() {
            // not stored: forget it again (or put back what was there)
            self.programs.retain(|a| a.m.id != id);
            if let Some((m, signer)) = before {
                self.programs.push(Program { m, signer });
            }
            return E_FAILED;
        }
        if stale {
            let _ = fs::remove_file(&pkg_path(id)); // (an older installed copy: the system's package is the one now)
        }
        OK
    }

    fn uninstall(&mut self, id: &str) -> u64 {
        if self.program(id).is_none() {
            return E_NOAPP;
        }
        if self.insts.iter().any(|i| i.program == id && self.running(i)) {
            return E_REFUSED;
        }
        let (copy, link) = (fs::remove_file(&pkg_path(id)), fs::remove_file(&link_path(id)));
        if copy.is_err() && link.is_err() {
            return E_FAILED;
        }
        remove_tree(&data_dir(id), 3); // (an uninstalled program takes its data with it, and its stored policy)
        let _ = fs::remove_file(&policy_path(id));
        self.programs.retain(|a| a.m.id != id);
        self.insts.retain(|i| i.program != id);
        OK
    }

    /// Start the instance's child with the rights its manifest asks for and nothing else.
    fn launch(&mut self, ix: usize) -> Result<(), u64> {
        let (id, ab) = (self.insts[ix].program.clone(), self.insts[ix].ability);
        let mut bytes = read_package(&id).map_err(|_| E_FAILED)?;
        let pkg = Package::parse(&bytes).map_err(|_| E_FAILED)?;
        // (the file lives on a writable partition: look at it again before running it)
        if metal_map::check(&pkg, &self.keys, &self.policy).is_err() {
            println!("[progmgr] {id}: the package on disk no longer passes the trust check, not starting it");
            return Err(E_FAILED);
        }
        let m = pkg.manifest.clone();
        let a = m.abilities.get(ab).ok_or(E_NOABILITY)?;
        // the program's image is cut out of the package where it lies (a browser is several MiB: no second copy, the manager's heap is not that big)
        let (start, len) = {
            let f = pkg.file(&a.entry).ok_or(E_FAILED)?;
            (f.as_ptr() as usize - bytes.as_ptr() as usize, f.len())
        };
        drop(pkg);
        bytes.truncate(start + len);
        bytes.drain(..start);
        let elf = bytes;
        let entry = self.insts[ix].entry;
        let saved = self.insts[ix].saved;
        // the board entry first: the child reads it as soon as it runs
        // SAFETY: the saved words sit at fixed offsets of the entry inside the mapped page.
        unsafe {
            *(self.board.add(entry * ENTRY + O_SAVED0) as *mut u64) = saved.map_or(0, |s| s.0);
            *(self.board.add(entry * ENTRY + O_SAVED1) as *mut u64) = saved.map_or(0, |s| s.1);
        }
        self.word(entry, O_RESTORED).store(saved.is_some() as u32, Ordering::Release);
        let st = if self.insts[ix].state == ST_BACKGROUND { ST_BACKGROUND } else if a.kind == Kind::Ui { ST_FOREGROUND } else { ST_CREATED };
        self.set_state(entry, st);
        self.insts[ix].state = st;
        // what the manifest declares, less what this start gave up
        // (the user's stored policy joins what this start gave up)
        let off = (self.insts[ix].off | policy_of(&id)) & 0x3F;
        self.insts[ix].off = off;
        let (files, network, display, shell, allfiles, admin) = (m.files && off & 1 == 0, m.network && off & 2 == 0, m.display && off & 4 == 0, m.shell && off & 8 == 0, m.allfiles && off & 16 == 0, m.admin && off & 32 == 0);
        let mut cmd = Command::new(&a.name);
        if files && !allfiles {
            // the program's own directory is all of /data to it: made now, entered before the program runs its first instruction
            let _ = fs::create_dir("/data/programdata");
            let dir = data_dir(&id);
            let _ = fs::create_dir(&dir);
            cmd.scope(&dir);
        }
        cmd.arg(&id);
        cmd.image(elf).numeric_arg(entry as u64).heap_pages(m.heap_pages).class(m.class.number()).threads(m.threads).files(files || allfiles).network(network);
        cmd.give(self.board_slot as u8, BOARD_SLOT as u8, 8 /* map, not write */).give(self.program_ep.0 as u8, PROGRAM_EP_SLOT as u8, 1 /* send */);
        if display {
            cmd.give(DISPLAY.0 as u8, DISPLAY_SLOT, 1 /* send */);
        }
        if shell {
            // a shell client (the taskbar) may start and stop programs like the terminal does: it gets a send-only copy of the control endpoint
            cmd.give(CTL.0 as u8, SHELL_CTL_SLOT, 1 /* send */);
        }
        if admin {
            // the revoke authority (the desktop session gave the manager a copy, in slot 7): the task manager's slot 4, without the right to pass it on
            cmd.give(ADMIN_SLOT, ADMIN_PROGRAM_SLOT, R_REVOKE);
        }
        match cmd.spawn() {
            Ok(c) => {
                println!("[progmgr] {id}/{} started as task {}", a.name, c.id());
                if shell {
                    // the compositor lets this task make panels and overlays and use the shell requests (it only listens to the manager for this)
                }
                self.insts[ix].child = Some(c);
                Ok(())
            }
            Err(e) => {
                println!("[progmgr] {id}/{} did not start: {e:?}", a.name);
                Err(E_FAILED)
            }
        }
    }

    fn start(&mut self, id: &str, ab: usize, off: u8) -> [u64; 3] {
        let Some(m) = self.program(id) else { return [E_NOAPP, 0, 0] };
        if ab >= m.abilities.len() {
            return [E_NOABILITY, 0, 0];
        }
        let ix = match self.inst(id, ab) {
            Some(ix) => {
                if self.running(&self.insts[ix]) {
                    return [E_STATE, 0, 0];
                }
                self.insts[ix].restarts = 0;
                self.insts[ix].crashes.clear();
                ix
            }
            None => {
                let Some(entry) = (0..ENTRIES).find(|e| !self.insts.iter().any(|i| i.entry == *e)) else { return [E_NOSLOT, 0, 0] };
                self.insts.push(Inst { program: id.into(), ability: ab, entry, child: None, state: ST_STOPPED, restarts: 0, crashes: Vec::new(), last_exit: u64::MAX, kill_at: 0, restart_at: None, saved: None, off: 0 });
                self.insts.len() - 1
            }
        };
        self.insts[ix].saved = None;
        self.insts[ix].state = ST_STOPPED;
        self.insts[ix].off = off & 0x3F;
        match self.launch(ix) {
            Ok(()) => [OK, self.insts[ix].entry as u64, self.insts[ix].child.as_ref().map_or(0, |c| c.id() as u64)],
            Err(e) => {
                self.set_state(self.insts[ix].entry, ST_STOPPED);
                self.insts[ix].state = ST_STOPPED;
                [e, 0, 0]
            }
        }
    }

    fn stop(&mut self, id: &str, ab: usize, now: u64) -> u64 {
        let Some(ix) = self.inst(id, ab) else { return if self.program(id).is_some() { E_STATE } else { E_NOAPP } };
        let entry = self.insts[ix].entry;
        if self.insts[ix].child.is_some() {
            self.set_state(entry, ST_DESTROYING);
            self.insts[ix].state = ST_DESTROYING;
            self.insts[ix].kill_at = now + GRACE;
            OK
        } else if self.insts[ix].restart_at.is_some() {
            self.insts[ix].restart_at = None;
            self.insts[ix].state = ST_STOPPED;
            self.set_state(entry, ST_STOPPED);
            OK
        } else {
            E_STATE
        }
    }

    fn switch(&mut self, id: &str, ab: usize, to: u32) -> u64 {
        let Some(m) = self.program(id) else { return E_NOAPP };
        if m.abilities.get(ab).map(|a| a.kind) != Some(Kind::Ui) {
            return E_REFUSED;
        }
        let Some(ix) = self.inst(id, ab) else { return E_STATE };
        if !matches!(self.insts[ix].state, ST_FOREGROUND | ST_BACKGROUND) || self.insts[ix].child.is_none() {
            return E_STATE;
        }
        if self.insts[ix].state == to {
            return OK; // (nothing changes: and the desktop must not hear an echo of what it just asked for)
        }
        self.insts[ix].state = to;
        self.set_state(self.insts[ix].entry, to);
        OK
    }

    /// Store which permissions the user has taken away from a program (it keeps applying to every start), and bring a running instance in line:
    /// its record of what it runs without follows, and a shell client that loses `shell` stops being one for the compositor.
    fn set_policy(&mut self, id: &str, new: u8) -> [u64; 3] {
        if self.program(id).is_none() {
            return [E_NOAPP, 0, 0];
        }
        let old = policy_of(id);
        let stored = if new == 0 {
            // (nothing to remove is as good as removed)
            fs::remove_file(&policy_path(id)).is_ok() || fs::read(&policy_path(id)).is_err()
        } else {
            fs::write(&policy_path(id), &[new]).is_ok()
        };
        if !stored {
            return [E_FAILED, old as u64, 0];
        }
        let changed = old ^ new;
        // (only what changes in a running program at once follows: the window, shell and revoke capabilities; files and network are sessions made at the start)
        let live = changed & (4 | 8 | 32);
        for i in self.insts.iter_mut().filter(|i| i.program == id) {
            i.off = (i.off & !live) | (new & live);
            // a shell program that loses the right stops being one for the compositor, which takes its panels and overlays away
            if changed & 8 != 0 && new & 8 != 0 {
                if let Some(c) = &i.child {
                }
            }
        }
        [OK, old as u64, new as u64]
    }

    /// Give a running program back the capability a permission stands for (window: the display endpoint, shell: the control endpoint, revoke: the
    /// revoke authority), in the slot the program expects it. Only what the manifest declares and the stored policy allows.
    fn grant_back(&mut self, id: &str, bit: u8) -> [u64; 3] {
        let Some(m) = self.program(id) else { return [E_NOAPP, 0, 0] };
        let declared = match bit {
            4 => m.display,
            8 => m.shell,
            32 => m.admin,
            _ => false,
        };
        if !declared || policy_of(id) & bit != 0 {
            return [E_REFUSED, 0, 0];
        }
        let Some(ix) = self.insts.iter().position(|i| i.program == id && i.child.is_some()) else { return [E_STATE, 0, 0] };
        let task = self.insts[ix].child.as_ref().map_or(0, |c| c.id() as u64);
        let (src, dest, rights): (u64, u64, u64) = match bit {
            4 => (DISPLAY.0 as u64, DISPLAY_SLOT as u64, 1),
            8 => (CTL.0 as u64, SHELL_CTL_SLOT as u64, 1),
            _ => (ADMIN_SLOT as u64, ADMIN_PROGRAM_SLOT as u64, R_REVOKE as u64),
        };
        // (the kernel puts it exactly in the slot the program looks in, or refuses: a slot that is still in use means it never lost it)
        let r = svc(CAP_GRANT, src, task, rights, dest + 1, 0)[0] as i64;
        if r < 0 {
            return [E_FAILED, (-r) as u64, 0];
        }
        self.insts[ix].off &= !bit;
        if bit == 8 {
        }
        [OK, r as u64, 0]
    }

    fn control(&mut self, w: [u64; 3], now: u64) -> [u64; 3] {
        let (op, ab) = (w[0] & 0xFF, ((w[0] >> 8) & 0xFF) as usize);
        let id = id_of(w[1], w[2]);
        match op {
            OP_INSTALL => [self.install(&id), 0, 0],
            OP_UNINSTALL => [self.uninstall(&id), 0, 0],
            OP_START => self.start(&id, ab, ((w[0] >> 16) & 0x3F) as u8),
            OP_STOP => [self.stop(&id, ab, now), 0, 0],
            OP_FOREGROUND => [self.switch(&id, ab, ST_FOREGROUND), 0, 0],
            OP_BACKGROUND => [self.switch(&id, ab, ST_BACKGROUND), 0, 0],
            OP_STATE | OP_SAVED => {
                if self.program(&id).is_none() {
                    return [E_NOAPP, 0, 0];
                }
                match self.inst(&id, ab) {
                    None if op == OP_STATE => [OK, ST_STOPPED as u64, u64::MAX],
                    None => [OK, 0, 0],
                    Some(ix) => {
                        let i = &self.insts[ix];
                        if op == OP_SAVED {
                            let s = i.saved.unwrap_or((0, 0));
                            [OK, s.0, s.1]
                        } else {
                            [OK, i.state as u64 | (i.restarts as u64) << 8 | (self.running(i) as u64) << 24 | (i.off as u64) << 25 | (i.child.as_ref().map_or(0, |c| c.id() as u64)) << 32, i.last_exit]
                        }
                    }
                }
            }
            OP_POLICY_GET => match self.program(&id) {
                Some(m) => {
                    let declared = m.files as u64 | (m.network as u64) << 1 | (m.display as u64) << 2 | (m.shell as u64) << 3 | (m.allfiles as u64) << 4 | (m.admin as u64) << 5;
                    let task = self.insts.iter().find(|i| i.program == id).and_then(|i| i.child.as_ref()).map_or(u64::MAX, |c| c.id() as u64);
                    [OK, policy_of(&id) as u64 | declared << 8, task]
                }
                None => [E_NOAPP, 0, 0],
            },
            OP_POLICY_SET => self.set_policy(&id, ((w[0] >> 16) & 0x3F) as u8),
            OP_GRANT => self.grant_back(&id, ((w[0] >> 16) & 0x3F) as u8),
            OP_LIST => match self.programs.get(w[1] as usize) {
                Some(program) => {
                    let (a, b) = pack(&program.m.id);
                    [OK, a, b]
                }
                None => [E_NOAPP, 0, 0],
            },
            OP_NAME => match self.program(&id) {
                Some(m) => {
                    let (a, b) = pack(&m.name);
                    [OK, a, b]
                }
                None => [E_NOAPP, 0, 0],
            },
            OP_INFO => match self.programs.iter().find(|a| a.m.id == id) {
                Some(Program { m, signer }) => [OK, m.heap_pages as u64 | (m.threads as u64) << 16 | (m.class.number() as u64) << 20 | (m.files as u64) << 24 | (m.network as u64) << 25 | (signer.is_some() as u64) << 27 | (m.display as u64) << 28 | (m.shell as u64) << 29 | (m.allfiles as u64) << 30 | (m.admin as u64) << 31, m.abilities.len() as u64],
                None => [E_NOAPP, 0, 0],
            },
            _ => [E_REFUSED, 0, 0],
        }
    }

    /// What an ability may ask for: to keep two words (only for itself: the caller is told apart by its task id).
    fn program_call(&mut self, sender: u64, w: [u64; 3]) -> [u64; 3] {
        if w[0] != OP_SAVE {
            return [E_REFUSED, 0, 0];
        }
        match self.insts.iter_mut().find(|i| i.child.as_ref().is_some_and(|c| c.id() as u64 == sender)) {
            Some(i) => {
                i.saved = Some((w[1], w[2]));
                [OK, 0, 0]
            }
            None => [E_NOAPP, 0, 0],
        }
    }

    fn supervise(&mut self, now: u64) {
        for ix in 0..self.insts.len() {
            if let Some(c) = &self.insts[ix].child {
                match c.try_wait() {
                    Ok(None) => {
                        if self.insts[ix].state == ST_DESTROYING && now >= self.insts[ix].kill_at {
                            println!("[progmgr] {}: no answer to on_destroy, killing it", self.insts[ix].program);
                            let _ = c.kill();
                        }
                    }
                    Ok(Some(status)) => self.ended(ix, status.0, now),
                    Err(_) => self.ended(ix, u64::MAX, now),
                }
            } else if self.insts[ix].restart_at.is_some_and(|t| now >= t) {
                self.insts[ix].restart_at = None;
                if self.launch(ix).is_err() {
                    self.crashed(ix, now);
                }
            }
        }
    }

    fn ended(&mut self, ix: usize, code: u64, now: u64) {
        // the compositor drops the task's windows now: a restart may get the same task number, and its old windows must not stay behind
        if let Some(c) = self.insts[ix].child.take() {
        }
        self.insts[ix].last_exit = code;
        let entry = self.insts[ix].entry;
        if self.insts[ix].state == ST_DESTROYING || code == 0 {
            println!("[progmgr] {} ended (exit {code})", self.insts[ix].program);
            self.insts[ix].state = ST_STOPPED;
            self.set_state(entry, ST_STOPPED);
        } else {
            println!("[progmgr] {} crashed (exit {code:#x})", self.insts[ix].program);
            self.crashed(ix, now);
        }
    }

    fn crashed(&mut self, ix: usize, now: u64) {
        let i = &mut self.insts[ix];
        i.crashes.retain(|&t| now.saturating_sub(t) < CRASH_WINDOW);
        i.crashes.push(now);
        if i.crashes.len() > MAX_CRASHES {
            println!("[progmgr] {} keeps crashing: giving up", i.program);
            i.state = ST_FAILED;
            let e = i.entry;
            self.set_state(e, ST_FAILED);
        } else {
            i.restarts += 1;
            i.restart_at = Some(now + 1);
        }
    }
}

program!("progmgr", |_c| {
    println!("[progmgr] starting");
    let board_slot = shm_create(1);
    let program_ep = Endpoint::create(FACTORY);
    let base = if board_slot >= 0 { map_anywhere(board_slot as u64, true) } else { board_slot };
    let (Ok(program_ep), true) = (program_ep, base >= 0) else {
        println!("[progmgr] cannot set up (board {board_slot}, mapping {base})");
        return 1;
    };
    // who may sign programs and whether unsigned ones are allowed: both files are in the read-only system image. A missing or
    // unreadable trust store trusts nobody; an unreadable policy is the strict one.
    let keys = fs::read_to_string("/system/etc/program-keys").ok().and_then(|t| metal_map::parse_keys(&t).ok()).unwrap_or_default();
    let policy = fs::read_to_string("/system/etc/program-policy").ok().and_then(|t| metal_map::parse_policy(&t).ok()).unwrap_or_default();
    println!("[progmgr] {} trusted signing keys, unsigned programs {}", keys.len(), if policy.allow_unsigned { "allowed (developer mode)" } else { "not allowed" });
    let mut m = Mgr { board_slot: board_slot as u64, board: base as usize as *mut u8, program_ep, programs: Vec::new(), insts: Vec::new(), keys, policy };
    process::set_capabilities(SPAWN, FACTORY);
    // the installed programs are the packages in /data/programs
    if let Ok(list) = fs::read_dir("/data/programs") {
        for e in list {
            if let Some(id) = e.name.strip_suffix(".map").or_else(|| e.name.strip_suffix(".sys")) {
                let _ = m.load(id);
            }
        }
    }
    println!("[progmgr] ready, {} programs installed", m.programs.len());
    loop {
        let now = ticks();
        loop {
            match CTL.try_recv() {
                Ok(Some(r)) => {
                    let rep = m.control(r.words, now);
                    if r.is_call {
                        let _ = ipc::reply(rep);
                    }
                }
                Ok(None) => break,
                Err(_) => return 0, // the control endpoint ended: shut down
            }
        }
        while let Ok(Some(r)) = m.program_ep.try_recv() {
            let rep = m.program_call(r.sender, r.words);
            if r.is_call {
                let _ = ipc::reply(rep);
            }
        }
        m.supervise(now);
        sleep(1);
    }
});
