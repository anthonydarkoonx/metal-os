#![no_std]
#![no_main]
//! The task manager (`../../docs/DESKTOP.md` §11.1): `metal-taskmgr` on the machine. Programs from the program manager (STATE,
//! INFO, STOP, START, FOREGROUND / BACKGROUND), tasks from the kernel's task table (`SYSINFO` 3, 5, 7, 10) and its scheduler
//! statistics (`SCHED` statistics: processor time and use per task and per core).
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use metal_compositor_os::OsTransport;
use metal_desktop_proto::client::Connection;
use metal_programs::desk;
use metal_programs::*;
use metal_taskmgr::{CapRow, Perf, ProgState, ProgramRow, ServiceRow, System, TaskManager, TaskRow};
use metal_ui::MemPlatform;

const MAX_TASKS: u64 = 64;
const PERMS: [&str; 6] = ["files", "network", "display", "shell", "all files", "admin"];
const CLASSES: [&str; 3] = ["interactive", "background", "heavy"];

struct Machine {
    hz: u64,
    /// Capabilities held per task, counted every few seconds (32 SYSINFO calls a task).
    caps: [u32; MAX_TASKS as usize],
    caps_at: u64,
    /// The installed programs (two calls to the manager each), read again every few seconds.
    installed: Vec<(String, String)>,
    installed_at: u64,
    /// The last sample of busy time (counter ticks) per task and per core, and when it was taken: CPU use is the busy time since
    /// the last sample over the time since it (the scheduler's utilisation estimate is for choosing frequencies, and counts
    /// short frequent wake-ups high).
    task_busy: [u64; MAX_TASKS as usize],
    task_pct: [u8; MAX_TASKS as usize],
    tasks_at: u64,
    core_busy: Vec<u64>,
    cores_at: u64,
}

/// How long counted things are kept.
const RECOUNT_MS: u64 = 5000;

/// Busy ticks over elapsed ticks, as a percentage.
fn share(busy: u64, elapsed: u64) -> u8 {
    if elapsed == 0 {
        0
    } else {
        (busy.saturating_mul(100) / elapsed).min(100) as u8
    }
}

impl Machine {
    /// Every task's CPU use since the last sample (taken at most once a refresh).
    fn sample_tasks(&mut self) {
        let now = desk::counter_now();
        let elapsed = now.saturating_sub(self.tasks_at);
        if self.tasks_at != 0 && elapsed < self.hz / 4 {
            return; // (sampled a moment ago: the programs list and the tasks list share one sample)
        }
        for t in 0..MAX_TASKS as usize {
            let ticks = desk::task_cpu(t as u64).map_or(0, |c| c.0);
            self.task_pct[t] = if self.tasks_at == 0 || ticks < self.task_busy[t] { 0 } else { share(ticks - self.task_busy[t], elapsed) };
            self.task_busy[t] = ticks;
        }
        self.tasks_at = now;
    }

    fn installed(&mut self) -> Vec<(String, String)> {
        let now = desk::now_ms();
        if self.installed.is_empty() || now.saturating_sub(self.installed_at) >= RECOUNT_MS {
            self.installed = ctl::installed();
            self.installed_at = now;
        }
        self.installed.clone()
    }
}


fn pages_of(t: u64) -> u64 {
    desk::sysinfo(7, t).map_or(0, |r| r[1])
}

impl System for Machine {
    fn programs(&mut self) -> Vec<ProgramRow> {
        self.sample_tasks();
        self.installed()
            .into_iter()
            .map(|(id, name)| {
                let st = desk::state(&id);
                let info = desk::info(&id);
                let (state, restarts, task, off) = match st {
                    Some((s, r, running, off, task, _)) => (
                        match s {
                            5 => ProgState::Failed,
                            4 => ProgState::Stopping,
                            1..=3 if running => ProgState::Running,
                            _ if running => ProgState::Starting,
                            _ => ProgState::Stopped,
                        },
                        r,
                        task,
                        off,
                    ),
                    None => (ProgState::Stopped, 0, None, 0),
                };
                let (heap, threads, class, declared, _) = info.unwrap_or((0, 0, 0, 0, false));
                let effective = declared & !off;
                let cpu = task.map_or(0, |t| self.task_pct.get(t as usize).copied().unwrap_or(0));
                ProgramRow {
                    name,
                    state,
                    restarts,
                    class: String::from(CLASSES.get(class as usize).copied().unwrap_or("?")),
                    cpu,
                    pages: task.map_or(0, pages_of),
                    heap_limit: heap,
                    threads,
                    permissions: (0..6).filter(|b| effective & 1 << b != 0).map(|b| String::from(PERMS[b])).collect(),
                    foreground: st.is_some_and(|s| s.0 == 2),
                    task: task.map(|t| t as u32),
                    id,
                }
            })
            .collect()
    }

    fn tasks(&mut self) -> Vec<TaskRow> {
        let now = desk::now_ms();
        let recount = self.caps_at == 0 || now.saturating_sub(self.caps_at) >= RECOUNT_MS;
        if recount {
            self.caps_at = now.max(1);
        }
        self.sample_tasks();
        let mut out = Vec::new();
        for t in 0..MAX_TASKS {
            let Some((name, code, _restarts, _role)) = desk::task_row(t) else { continue };
            if code == 7 {
                continue; // ended, not collected yet
            }
            let (program, parent) = match desk::sysinfo(5, t) {
                Some(r) => (Some(r[3] as u32), (r[4] != 255).then_some(r[4] as u32)),
                None => (None, None),
            };
            let (ticks, _util, level) = desk::task_cpu(t).unwrap_or((0, 0, 0));
            if recount {
                self.caps[t as usize] = (0..32).filter(|&s| desk::cap_slot(t, s).is_some_and(|c| c.0 != 0)).count() as u32;
            }
            let caps = self.caps[t as usize];
            out.push(TaskRow {
                task: t as u32,
                name,
                program: program.filter(|&p| p as u64 != t),
                parent,
                class: String::from(["real time", "interactive", "background", "heavy"].get(level as usize).copied().unwrap_or("?")),
                state: String::from(desk::state_text(code)),
                cpu_ms: ticks * 1000 / self.hz,
                cpu: self.task_pct[t as usize],
                pages: pages_of(t),
                caps,
            });
        }
        out
    }

    fn services(&mut self) -> Vec<ServiceRow> {
        // the boot tasks with a role (drivers, file servers, the network): the kernel restarts them when they fail
        let mut out = Vec::new();
        for t in 0..MAX_TASKS {
            let Some((name, code, restarts, role)) = desk::task_row(t) else { continue };
            if role == 0 || code == 7 {
                continue;
            }
            out.push(ServiceRow { name, role: role as u8, state: String::from(desk::state_text(code)), restarts: restarts as u32, restartable: false });
        }
        out
    }

    fn perf(&mut self) -> Perf {
        let s = svc(SYSINFO, 0, 0, 0, 0, 0);
        // (busy time since the last sample, over the time since it)
        let now = desk::counter_now();
        let elapsed = now.saturating_sub(self.cores_at);
        let n = s[4].max(1) as usize;
        let busy: Vec<u64> = (0..n as u64).map(|c| desk::core_busy(c).unwrap_or(0)).collect();
        let per_core = if self.core_busy.len() == n && self.cores_at != 0 {
            busy.iter().zip(&self.core_busy).map(|(b, p)| share(b.saturating_sub(*p), elapsed)).collect()
        } else {
            alloc::vec![0; n]
        };
        self.core_busy = busy;
        self.cores_at = now;
        let (total, free) = (s[2] * 4, s[3] * 4);
        Perf { per_core, mem_total_kib: total, mem_used_kib: total.saturating_sub(free), mem_compressed_kib: 0, mem_swapped_kib: 0, net: Vec::new() }
    }

    fn capabilities(&mut self, task: u32) -> Vec<CapRow> {
        (0..32u64)
            .filter_map(|s| {
                let (kind, obj, rights, _node) = desk::cap_slot(task as u64, s)?;
                if kind == 0 {
                    return None;
                }
                let name = desk::object(kind, obj).map(|o| o.0).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("#{obj}"));
                Some(CapRow { slot: s as u8, kind: String::from(desk::KINDS.get(kind as usize).copied().unwrap_or("?")), rights: desk::rights_text(kind, rights), object: name })
            })
            .collect()
    }

    fn stop(&mut self, id: &str) -> Result<(), String> {
        if id == "desktop" {
            return Err(String::from("the desktop is not stopped from here"));
        }
        if desk::stop(id) {
            Ok(())
        } else {
            Err(String::from("the program manager refused (is it running?)"))
        }
    }
    fn restart(&mut self, id: &str) -> Result<(), String> {
        self.stop(id)?;
        // (the manager stops it at its next tick; START is refused until it has)
        for _ in 0..50 {
            if desk::state(id).is_some_and(|s| !s.2) {
                break;
            }
            sleep(2);
        }
        if ctl::start(id) {
            Ok(())
        } else {
            Err(String::from("it stopped, but did not start again"))
        }
    }
    fn set_foreground(&mut self, id: &str, foreground: bool) -> Result<(), String> {
        if desk::set_foreground(id, foreground) {
            Ok(())
        } else {
            Err(String::from("the program manager refused"))
        }
    }
    fn open_capabilities(&mut self, _task: u32) -> Result<(), String> {
        // (START has no argument yet, proposal P3: the capabilities manager opens on its first page)
        if ctl::start("capmgr") {
            Ok(())
        } else {
            Err(String::from("the capabilities manager is not installed"))
        }
    }
}

fn main() -> u64 {
    let mut t = OsTransport::new(2);
    let mut conn = match Connection::hello(&mut t) {
        Ok(c) => c,
        Err(e) => {
            println!("[taskmgr] no display: error {e}");
            return 1;
        }
    };
    let mut buf = [0u8; 256];
    let theme = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])).unwrap_or(metal_ui::theme::LIGHT);
    let machine = Machine { hz: desk::counter_hz(), caps: [0; MAX_TASKS as usize], caps_at: 0, installed: Vec::new(), installed_at: 0, task_busy: [0; MAX_TASKS as usize], task_pct: [0; MAX_TASKS as usize], tasks_at: 0, core_busy: Vec::new(), cores_at: 0 };
    let mut app = match TaskManager::open(&mut conn, &mut t, Box::new(machine), &theme, desk::now_ms()) {
        Ok(a) => a,
        Err(e) => {
            println!("[taskmgr] cannot open its window: error {e}");
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
        app.update(&mut conn, &mut t, p.now);
        if app.win.close_requested {
            conn.bye(&mut t);
            return 0;
        }
        // (it refreshes once a second)
        conn.wait(&mut t, seen, 500);
    }
}

program!("taskmgr", |_c| main());
