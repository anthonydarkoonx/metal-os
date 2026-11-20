#![no_std]
#![no_main]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use metallium_programs::ipc::{self, Endpoint};
use metallium_programs::process::{Child, Command};
use metallium_programs::*;

// The session service (docs/MULTIUSER-PLAN.md, phase 1). It keeps the accounts and starts a user's session confined to the user's home.
//
// Slot 0: its control endpoint (receive; whoever holds the send right is the greeter), slot 5: the endpoint factory, slot 6: the spawn capability (with
// file access). Accounts are lines `uid:name:role:iterations:salt:hash` in `/data/accounts`, which only this program (unconfined) can see: a session's
// `/data` is `/data/home/<name>`. The credential here is a PBKDF2-HMAC-SHA512 hash: phase 2 replaces it with a key slot of the user's own encryption domain.
//
// Protocol (three words a message): `BEGIN` names the action and the user, then the secret follows in chunks of 16 bytes (as for the data volume's lock):
//   BEGIN  w0 = 1 | action << 8 | role << 16 | acting session << 32, w1 w2 = the user name (up to 16 bytes, NUL padded)
//   SECRET w0 = 2 | total length << 8 | offset << 24, w1 w2 = 16 bytes of the passphrase at `offset`; the last chunk runs the action
//   LOGOUT w0 = 3, w1 = session id
//   STATUS w0 = 4 -> [number of users, number of sessions, 0]
// Replies: w0 = status (0 ok, 4 = more chunks wanted, see below), w1 = the user id (CREATE) or the session id (LOGIN).
const OP_BEGIN: u64 = 1;
const OP_SECRET: u64 = 2;
const OP_LOGOUT: u64 = 3;
const OP_STATUS: u64 = 4;
const ACT_CREATE: u64 = 1;
const ACT_LOGIN: u64 = 2;

const OK: u64 = 0;
const E_BAD: u64 = 1; // malformed request, or a name that is not allowed
const E_EXISTS: u64 = 2; // CREATE: the name is taken
const E_MORE: u64 = 4; // SECRET: send the next chunk
const E_DENIED: u64 = 5; // CREATE: the acting session is not an admin's (once there is an admin)
const E_FAILED: u64 = 6; // storage or start failure
const E_LOGIN: u64 = 7; // LOGIN: no such user or wrong passphrase (one answer for both: no way to find out which names exist)
const E_BUSY: u64 = 8; // LOGIN: that user already has a session

const ITERATIONS: u32 = 20_000;
/// Files and directories a home may hold (the account holder's own quota; programs under it have theirs, never more).
const HOME_QUOTA: u64 = 64;

const R_SPAWN: u16 = 1 << 12;
const R_SPAWN_FS: u16 = 1 << 13;
const R_EP_CREATE: u16 = 1 << 14;
const FACTORY: u8 = 5;
const SPAWN: u8 = 6;

struct Account {
    uid: u32,
    name: String,
    admin: bool,
    iterations: u32,
    salt: [u8; 16],
    hash: [u8; 32],
}

struct Session {
    id: u64,
    uid: u32,
    child: Child,
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn unhex(s: &str, out: &mut [u8]) -> bool {
    if s.len() != out.len() * 2 {
        return false;
    }
    for (i, o) in out.iter_mut().enumerate() {
        match u8::from_str_radix(&s[2 * i..2 * i + 2], 16) {
            Ok(b) => *o = b,
            Err(_) => return false,
        }
    }
    true
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 16 && n.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_') && n.as_bytes()[0].is_ascii_lowercase()
}

fn derive(pass: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    metallium_crypt::pbkdf2_hmac_sha512(pass, salt, iterations, &mut h);
    h
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

struct Svc {
    accounts: Vec<Account>,
    sessions: Vec<Session>,
    next_session: u64,
    /// The request being assembled: (action, role, acting session, name, secret so far).
    pending: Option<(u64, bool, u64, String, Vec<u8>)>,
}

impl Svc {
    fn load() -> Vec<Account> {
        let mut v = Vec::new();
        let Ok(t) = fs::read_to_string("/data/accounts") else { return v };
        for line in t.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() != 6 {
                continue;
            }
            let (mut salt, mut hash) = ([0u8; 16], [0u8; 32]);
            if let (Ok(uid), Ok(role), Ok(iterations), true, true) = (f[0].parse(), f[2].parse::<u8>(), f[3].parse(), unhex(f[4], &mut salt), unhex(f[5], &mut hash)) {
                v.push(Account { uid, name: String::from(f[1]), admin: role == 1, iterations, salt, hash });
            }
        }
        v
    }

    fn save(&self) -> bool {
        let mut t = String::new();
        for a in &self.accounts {
            t.push_str(&format!("{}:{}:{}:{}:{}:{}\n", a.uid, a.name, a.admin as u8, a.iterations, hex(&a.salt), hex(&a.hash)));
        }
        fs::write("/data/accounts", t.as_bytes()).is_ok() && fs::sync().is_ok()
    }

    fn create(&mut self, name: &str, want_admin: bool, acting: u64, pass: &[u8]) -> [u64; 3] {
        if !valid_name(name) || pass.is_empty() {
            return [E_BAD, 0, 0];
        }
        if self.accounts.iter().any(|a| a.name == name) {
            return [E_EXISTS, 0, 0];
        }
        // the first account is the administrator's; after that only an admin's live session may add accounts
        let first = self.accounts.is_empty();
        if !first {
            let by_admin = self.sessions.iter().any(|s| s.id == acting && self.accounts.iter().any(|a| a.uid == s.uid && a.admin));
            if !by_admin {
                return [E_DENIED, 0, 0];
            }
        }
        let mut salt = [0u8; 16];
        if random::fill(&mut salt).is_err() {
            return [E_FAILED, 0, 0];
        }
        let uid = self.accounts.iter().map(|a| a.uid + 1).max().unwrap_or(1);
        self.accounts.push(Account { uid, name: String::from(name), admin: first || want_admin, iterations: ITERATIONS, salt, hash: derive(pass, &salt, ITERATIONS) });
        if !self.save() {
            self.accounts.pop();
            return [E_FAILED, 0, 0];
        }
        println!("[sessiond] account {name} created (uid {uid}{})", if first { ", the administrator" } else { "" });
        [OK, uid as u64, 0]
    }

    fn login(&mut self, name: &str, pass: &[u8]) -> [u64; 3] {
        let Some(a) = self.accounts.iter().find(|a| a.name == name) else {
            // (the same work is done for a name that does not exist: the answer is not faster)
            let _ = derive(pass, &[0u8; 16], ITERATIONS);
            return [E_LOGIN, 0, 0];
        };
        if !same(&derive(pass, &a.salt, a.iterations), &a.hash) {
            return [E_LOGIN, 0, 0];
        }
        let (uid, name) = (a.uid, a.name.clone());
        if self.sessions.iter().any(|s| s.uid == uid) {
            return [E_BUSY, 0, 0];
        }
        // the session: a program confined to the user's home (`/data` is `/data/home/<name>`), with a quota; the programs it starts are confined to the same directory
        let _ = fs::create_dir("/data/home");
        let home = format!("/data/home/{name}");
        let _ = fs::create_dir(&home);
        let mut cmd = Command::new("homeprobe");
        cmd.arg(&name).arg("probe").files(true).scope(&home).quota(HOME_QUOTA);
        cmd.give(FACTORY, 5, R_EP_CREATE).give(SPAWN, 6, R_SPAWN | R_SPAWN_FS).budget_pages(1000);
        let Ok(child) = cmd.spawn() else { return [E_FAILED, 0, 0] };
        self.next_session += 1;
        let id = self.next_session;
        println!("[sessiond] {name} logged in: session {id} (task {})", child.id());
        self.sessions.push(Session { id, uid, child });
        [OK, id, 0]
    }

    fn logout(&mut self, id: u64) -> [u64; 3] {
        let Some(i) = self.sessions.iter().position(|s| s.id == id) else { return [E_BAD, 0, 0] };
        let s = self.sessions.remove(i);
        let _ = s.child.kill();
        println!("[sessiond] session {id} ended");
        [OK, 0, 0]
    }

    fn request(&mut self, w: [u64; 3]) -> [u64; 3] {
        match w[0] & 0xFF {
            OP_BEGIN => {
                let mut raw = [0u8; 16];
                raw[..8].copy_from_slice(&w[1].to_le_bytes());
                raw[8..].copy_from_slice(&w[2].to_le_bytes());
                let n = raw.iter().position(|&c| c == 0).unwrap_or(16);
                let Ok(name) = core::str::from_utf8(&raw[..n]) else { return [E_BAD, 0, 0] };
                let action = (w[0] >> 8) & 0xFF;
                if action != ACT_CREATE && action != ACT_LOGIN {
                    return [E_BAD, 0, 0];
                }
                self.pending = Some((action, (w[0] >> 16) & 0xFF == 1, (w[0] >> 32) & 0xFFFF_FFFF, String::from(name), Vec::new()));
                [OK, 0, 0]
            }
            OP_SECRET => {
                let (total, offset) = (((w[0] >> 8) & 0xFFFF) as usize, ((w[0] >> 24) & 0xFFFF) as usize);
                let Some((action, admin, acting, name, mut secret)) = self.pending.take() else { return [E_BAD, 0, 0] };
                if total == 0 || total > 128 || offset != secret.len() || offset % 16 != 0 || offset >= total {
                    return [E_BAD, 0, 0];
                }
                let mut chunk = [0u8; 16];
                chunk[..8].copy_from_slice(&w[1].to_le_bytes());
                chunk[8..].copy_from_slice(&w[2].to_le_bytes());
                secret.extend_from_slice(&chunk[..(total - offset).min(16)]);
                if secret.len() < total {
                    self.pending = Some((action, admin, acting, name, secret));
                    return [E_MORE, 0, 0];
                }
                let r = if action == ACT_CREATE { self.create(&name, admin, acting, &secret) } else { self.login(&name, &secret) };
                secret.iter_mut().for_each(|b| *b = 0);
                r
            }
            OP_LOGOUT => self.logout(w[1]),
            OP_STATUS => [self.accounts.len() as u64, self.sessions.len() as u64, 0],
            _ => [E_BAD, 0, 0],
        }
    }
}

program!("sessiond", |_c| {
    process::set_capabilities(SPAWN as u64, FACTORY as u64);
    let mut svc = Svc { accounts: Svc::load(), sessions: Vec::new(), next_session: 0, pending: None };
    println!("[sessiond] up: {} account(s)", svc.accounts.len());
    let ctl = Endpoint(0);
    loop {
        match ctl.recv() {
            Ok(r) => {
                let rep = svc.request(r.words);
                if r.is_call {
                    let _ = ipc::reply(rep);
                }
            }
            Err(_) => return 0,
        }
    }
});
