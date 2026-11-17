//! What the desktop's programs (desktop, settings, files) ask the system: the time, the machine, the program manager. Plain system
//! calls and the control protocol (`docs/PROGRAMS.md`); the desktop's own crates live outside this tree (`../../metal-*`).
use alloc::string::String;

const CLOCK: u64 = 34;

/// Milliseconds since the machine started.
pub fn now_ms() -> u64 {
    // (CLOCK returns its value in the first register)
    crate::svc(CLOCK, 0, 0, 0, 0, 0)[0] / 1_000_000
}

/// Seconds since 1970 (UTC), if the machine has a real-time clock.
pub fn unix_seconds() -> Option<u64> {
    let ns = crate::svc(CLOCK, 1, 0, 0, 0, 0)[0] as i64;
    (ns > 0).then(|| ns as u64 / 1_000_000_000)
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn civil(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // (Howard Hinnant's days-to-civil)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y as u16, m as u8, d as u8, (rem / 3600) as u8, (rem / 60 % 60) as u8, (rem % 60) as u8)
}

/// The program a task belongs to, as the compositor names it (SYSINFO page 5).
pub fn program_of(task: u64) -> u64 {
    let r = crate::svc(crate::SYSINFO, 5, task, 0, 0, 0);
    if r[0] as i64 == 0 {
        r[3]
    } else {
        task
    }
}

fn pack(s: &str) -> (u64, u64) {
    let mut b = [0u8; 16];
    let n = s.len().min(16);
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
}

/// The task running a program, if it runs (the manager's POLICY_GET).
pub fn task_of(id: &str) -> Option<u64> {
    let (a, b) = pack(id);
    let (st, r) = crate::call(crate::ctl::SHELL_CTL_SLOT, [13, a, b]);
    (st == 0 && r[0] == 0 && r[2] != u64::MAX).then_some(r[2])
}

/// The tasks running a program's instances (the manager's TASKS).
pub fn tasks_of(id: &str) -> alloc::vec::Vec<u64> {
    let (a, b) = pack(id);
    let mut v = alloc::vec::Vec::new();
    for n in 0..16u64 {
        let (st, r) = crate::call(crate::ctl::SHELL_CTL_SLOT, [16 | n << 16, a, b]);
        if st != 0 || r[0] != 0 {
            break;
        }
        v.push(r[1]);
    }
    v
}

/// Install a package of the system image or of `/data/programs` by id (the manager's INSTALL): what the signature check said.
pub fn install(id: &str) -> Result<(), String> {
    let (a, b) = pack(id);
    let (st, r) = crate::call(crate::ctl::SHELL_CTL_SLOT, [1, a, b]);
    if st != 0 {
        return Err(alloc::format!("the program manager did not answer ({st})"));
    }
    Err(String::from(match r[0] {
        0 => return Ok(()),
        1 => "no package of that name in /system/usr/programs or /data/programs",
        2 => "the package is not valid",
        3 => "the package's id differs from its file name",
        4 => "the program is running",
        5 => "it could not be stored",
        6 => "it is not signed, and unsigned programs are not allowed (developer mode is off)",
        8 => "it is signed with a key this system does not trust",
        9 => "its signature does not match its contents",
        _ => "the program manager refused",
    }))
}

// ---- the program manager's control protocol, the rest of it (docs/PROGRAMS.md) ----

fn ctl_call(op: u64, id: &str) -> Option<[u64; 3]> {
    let (a, b) = pack(id);
    let (st, r) = crate::call(crate::ctl::SHELL_CTL_SLOT, [op, a, b]);
    (st == 0 && r[0] == 0).then_some(r)
}

/// STATE of a program's first instance: (state, restarts, running, permissions given up, task, last exit).
pub fn state(id: &str) -> Option<(u32, u32, bool, u8, Option<u64>, u64)> {
    let r = ctl_call(5, id)?;
    let w = r[1];
    let task = (w >> 32) as u64;
    let running = (w >> 24) & 1 != 0;
    Some(((w & 0xFF) as u32, ((w >> 8) & 0xFFFF) as u32, running, ((w >> 25) & 0x3F) as u8, (running && task != 0).then_some(task), r[2]))
}

/// What a program's manifest says: (heap pages, threads, class, permission bits declared, signed).
pub fn info(id: &str) -> Option<(u64, u32, u32, u8, bool)> {
    let w = ctl_call(10, id)?[1];
    let perms = ((w >> 24) & 1) | ((w >> 25) & 1) << 1 | ((w >> 28) & 1) << 2 | ((w >> 29) & 1) << 3 | ((w >> 30) & 1) << 4 | ((w >> 31) & 1) << 5;
    Some((w & 0xFFFF, ((w >> 16) & 0xF) as u32, ((w >> 20) & 0xF) as u32, perms as u8, (w >> 27) & 1 != 0))
}

pub fn stop(id: &str) -> bool {
    ctl_call(4, id).is_some()
}

pub fn set_foreground(id: &str, fg: bool) -> bool {
    ctl_call(if fg { 6 } else { 7 }, id).is_some()
}

/// Give a running program back the capability a permission bit stands for (GRANT).
pub fn grant(id: &str, bit: u8) -> Result<(), u64> {
    let (a, b) = pack(id);
    let (st, r) = crate::call(crate::ctl::SHELL_CTL_SLOT, [15 | (bit as u64) << 16, a, b]);
    if st != 0 {
        return Err(u64::MAX);
    }
    if r[0] == 0 {
        Ok(())
    } else {
        Err(r[0])
    }
}

// ---- the kernel's read-only pages (SYSINFO, SCHED statistics) ----

pub fn sysinfo(page: u64, arg: u64) -> Option<[u64; 6]> {
    let r = crate::svc(crate::SYSINFO, page, arg, 0, 0, 0);
    (r[0] as i64 == 0).then_some(r)
}

/// A task's row: (name, state code, restarts, role); `None` for a free slot.
pub fn task_row(t: u64) -> Option<(String, u64, u64, u64)> {
    let r = sysinfo(3, t)?;
    if r[3] == 0 {
        return None;
    }
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&r[1].to_le_bytes());
    b[8..].copy_from_slice(&r[2].to_le_bytes());
    let n = b.iter().position(|&c| c == 0).unwrap_or(16);
    Some((String::from_utf8_lossy(&b[..n]).into_owned(), r[3], r[4], r[5]))
}

pub fn state_text(code: u64) -> &'static str {
    match code {
        1 => "ready",
        2 => "waiting for a message",
        3 => "waiting for an answer",
        4 => "sleeping",
        5 => "waiting for a device",
        6 => "waiting on a ring",
        7 => "ended",
        8 => "waiting for a child",
        9 => "starting",
        10 => "waiting on a word",
        _ => "?",
    }
}

const SCHED: u64 = 23;
const SC_STATS: u64 = 4;

/// A task's processor use: (counter ticks run, utilisation 0..=1024, priority level).
pub fn task_cpu(t: u64) -> Option<(u64, u64, u64)> {
    let r = crate::svc(SCHED, SC_STATS, 1, t, 0, 0);
    (r[0] as i64 == 0).then(|| (r[1] + r[2], r[3], r[4]))
}

/// A core's utilisation 0..=1024.
pub fn core_util(c: u64) -> Option<u64> {
    let r = crate::svc(SCHED, SC_STATS, 3, c, 0, 0);
    (r[0] as i64 == 0).then_some(r[1])
}

/// A core's busy time so far, in counter ticks.
pub fn core_busy(c: u64) -> Option<u64> {
    let r = crate::svc(SCHED, SC_STATS, 3, c, 0, 0);
    (r[0] as i64 == 0).then_some(r[2])
}

/// The counter now (ticks since the machine started).
pub fn counter_now() -> u64 {
    crate::svc(SCHED, SC_STATS, 5, 0, 0, 0)[2]
}

/// The counter's frequency (ticks a second).
pub fn counter_hz() -> u64 {
    crate::svc(SCHED, SC_STATS, 5, 0, 0, 0)[1].max(1)
}

pub const KINDS: [&str; 10] = ["empty", "endpoint", "memory", "console", "interrupt", "memory control", "admin", "scheduling", "spawn", "factory"];

/// One capability slot of a task: (kind, object, rights, node); kind 0 = empty.
pub fn cap_slot(t: u64, slot: u64) -> Option<(u64, u64, u64, u64)> {
    let r = sysinfo(10, t | slot << 8)?;
    Some((r[1], r[2], r[3], r[4]))
}

/// The rights of a capability as words (what they mean depends on the kind).
pub fn rights_text(kind: u64, r: u64) -> String {
    let mut v: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    let names: &[(u64, &str)] = match kind {
        1 => &[(1, "send"), (2, "receive"), (4, "grant")],
        2 => &[(8, "map"), (16, "write"), (4, "grant")],
        5 => &[(1 << 6, "stats"), (1 << 7, "compact"), (1 << 8, "allocate"), (1 << 10, "swap"), (1 << 5, "pager"), (4, "grant")],
        6 => &[(1 << 9, "kill"), (1 << 10, "revoke"), (1 << 11, "power"), (4, "grant")],
        7 => &[(1 << 11, "real time"), (4, "grant")],
        8 => &[(1 << 12, "spawn"), (1 << 13, "files"), (1 << 15, "network"), (4, "grant")],
        9 => &[(1 << 14, "endpoints"), (4, "grant")],
        _ => &[(1, "send"), (2, "receive"), (4, "grant"), (32, "info")],
    };
    for (bit, n) in names {
        if r & bit != 0 {
            v.push(n);
        }
    }
    if v.is_empty() {
        String::from("-")
    } else {
        v.join(", ")
    }
}

/// What an object is: (name, size in pages or interrupt number, class, creator task).
pub fn object(kind: u64, obj: u64) -> Option<(String, u64, u64, Option<u64>)> {
    let r = sysinfo(12, kind | obj << 8)?;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&r[1].to_le_bytes());
    b[8..].copy_from_slice(&r[2].to_le_bytes());
    let n = b.iter().position(|&c| c == 0).unwrap_or(16);
    Some((String::from_utf8_lossy(&b[..n]).into_owned(), r[3], r[4], (r[5] != 255).then_some(r[5])))
}
