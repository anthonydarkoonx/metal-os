//! Scheduler tests: time slices, real-time reservations, work conservation across cores and
//! energy-aware placement. The conductor drives them; role 16 is a CPU burner that spins while a
//! flag in a shared page is raised and sleeps otherwise.
use crate::bench::{counter, frequency};
use crate::*;

const SCHED: u64 = 23;
const SC_RT_SET: u64 = 1;
const SC_RT_WAIT: u64 = 2;
const SC_RT_CLEAR: u64 = 3;
const SC_STATS: u64 = 4;
const E_FULL: i64 = -3;

const SPIN_VA: usize = 0x803B_0000;
/// Capability slots of the conductor.
const SPIN_SLOT: u64 = 24;
const RT_SLOT: u64 = 25;

/// Role 16: burn CPU while the flag is 1; while it is 3, only the burner whose task id the next word holds does.
pub fn spinner(tid: u64) -> ! {
    if mem_map(0, SPIN_VA, false) != 0 {
        println!("[spinner] cannot map the flag page");
        exit(1);
    }
    let flag = SPIN_VA as *const u64;
    let mut x = 1u64;
    loop {
        // SAFETY: SPIN_VA is the flag page mapped read-only (checked) at the top of `spinner`; volatile because the conductor changes it.
        let f = unsafe { read_volatile(flag) };
        // SAFETY: same mapped flag page; word 1 is inside it.
        if f != 1 && !(f == 3 && unsafe { read_volatile(flag.add(1)) } == tid) {
            sleep(8); // 12 polls a second: idle burners cost next to nothing
            continue;
        }
        for i in 0..20_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
        core::hint::black_box(x); // keeps the loop from being optimised away
    }
}

/// The otherwise idle `prog` (an Interactive task) as a hog while the flag page holds 2: a competitor above the
/// Background class, for the inheritance test.
pub fn interactive_hog() -> ! {
    if mem_map(2, SPIN_VA, false) != 0 {
        exit(1);
    }
    let flag = SPIN_VA as *const u64;
    let mut x = 1u64;
    loop {
        // SAFETY: SPIN_VA mapped (checked) at the top of `interactive_hog`; volatile because the conductor changes it.
        if unsafe { read_volatile(flag) } != 2 {
            sleep(8);
            continue;
        }
        for i in 0..20_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
        core::hint::black_box(x);
    }
}

/// A real-time task calling a Background server on a core that an Interactive hog never leaves: the server only
/// runs at all if it takes over the caller's urgency (one core only: with more the server would simply use another).
fn inheritance_test(freq: u64) {
    // SAFETY: SPIN_VA is mapped writable by `sched_tests` (checked), the only caller; the burners read it.
    unsafe { write_volatile(SPIN_VA as *mut u64, 2) };
    sleep(25); // the hog starts
    let core = rt_set(3_000, 20_000, 0);
    let (mut worst, mut sum, jobs) = (0u64, 0u64, 25u64);
    if core >= 0 {
        for _ in 0..jobs {
            sched_call(SC_RT_WAIT, 0, 0, 0, 0);
            let t0 = counter();
            let (st, _) = call(2, [crate::OP_PING, 0, 0]);
            let us = (counter() - t0) * 1_000_000 / freq;
            if st != 0 {
                worst = u64::MAX;
            }
            worst = worst.max(us);
            sum += us;
        }
        rt_clear();
    }
    // SAFETY: same writable flag page mapped by `sched_tests`.
    unsafe { write_volatile(SPIN_VA as *mut u64, 0) };
    println!("[client] SCHED real-time task calling a Background server while an Interactive task hogs the only core: worst {worst} us, mean {} us over {jobs} calls", sum / jobs);
    println!("[client] TEST a real-time caller's urgency reaches the server it calls (worst call {worst} us, limit 2000): {}", verdict(core >= 0 && worst < 2000));
}

fn sched_call(op: u64, a: u64, b: u64, c: u64, d: u64) -> [u64; 6] {
    svc(SCHED, op, a, b, c, d)
}

fn rt_set(runtime_us: u64, period_us: u64, core: u64) -> i64 {
    sched_call(SC_RT_SET, RT_SLOT, runtime_us, period_us, core)[0] as i64
}

fn rt_clear() {
    sched_call(SC_RT_CLEAR, 0, 0, 0, 0);
}

/// Statistics page `which`: x1..x5.
fn stats(which: u64, arg: u64) -> [u64; 5] {
    let r = sched_call(SC_STATS, which, arg, 0, 0);
    [r[1], r[2], r[3], r[4], r[5]]
}

fn set_burners(on: bool) {
    // SAFETY: same writable flag page mapped by `sched_tests`, which calls `set_burners`.
    unsafe { write_volatile(SPIN_VA as *mut u64, on as u64) };
}

fn burn_until(deadline: u64) {
    while counter() < deadline {}
}

/// (task id, ...) of every task running `role`.
fn tasks_of(role: u64, out: &mut [usize; 8]) -> usize {
    let mut n = 0;
    for t in 0..32u64 {
        let r = sysinfo_at(3, t);
        if r[0] == crate::E_INVAL as u64 {
            break;
        }
        if r[3] != 0 && r[3] != 7 && r[5] == role && n < out.len() {
            out[n] = t as usize;
            n += 1;
        }
    }
    n
}

/// CPU time a task has had so far, microseconds: (on efficiency cores, on performance cores).
fn cpu_us(tid: usize, freq: u64) -> (u64, u64) {
    let s = stats(1, tid as u64);
    (s[0] * 1_000_000 / freq, s[1] * 1_000_000 / freq)
}

fn my_tid() -> usize {
    // the conductor is task 0
    0
}

pub fn sched_tests() {
    if mem_map(SPIN_SLOT, SPIN_VA, true) != 0 {
        println!("[client] TEST scheduler: no flag page: FAIL");
        return;
    }
    let freq = frequency();
    let ncpus = svc(20, 0, 0, 0, 0, 0)[0] as usize;
    let mut burners = [0usize; 8];
    let nb = tasks_of(16, &mut burners);
    let mut worker = [0usize; 8];
    let nw = tasks_of(3, &mut worker);
    let mut hogs = [0usize; 16];
    let mut nh = 0;
    for &t in &burners[..nb] {
        hogs[nh] = t;
        nh += 1;
    }
    for &t in &worker[..nw] {
        hogs[nh] = t;
        nh += 1;
    }
    // the core real-time tests run on: where the Heavy work lives (a performance core if there is one)
    let rt_core: u64 = if ncpus >= 3 { 2 } else { 0 };

    // ---- 0. placement: one sustained-compute task alone stays on a performance core ---------------------
    if ncpus >= 3 && nw > 0 {
        let (l0, b0) = cpu_us(worker[0], freq);
        sleep(60);
        let (l1, b1) = cpu_us(worker[0], freq);
        println!("[client] TEST Heavy work stays on performance cores when they are not oversubscribed (ran {} ms there, {} ms on efficiency cores, at most 20% tolerated: a core can be busy with other work for a moment, and a task that waits 5 ms is given an efficiency core): {}", (b1 - b0) / 1000, (l1 - l0) / 1000, verdict((l1 - l0) * 5 <= b1 - b0 && b1 - b0 > 300_000));
    }

    // ---- 1. time slices, fairness and work conservation ------------------------------------------------
    set_burners(true);
    sleep(25); // let them start (they poll every 8 ticks) and spread out
    let window_ticks = 100;
    let before: [(u64, u64); 16] = core::array::from_fn(|i| if i < nh { cpu_us(hogs[i], freq) } else { (0, 0) });
    let c0 = stats(0, 0);
    let t0 = counter();
    sleep(window_ticks);
    let wall_us = (counter() - t0) * 1_000_000 / freq;
    let c1 = stats(0, 0);
    let mut shares = [0u64; 16];
    let mut big_us = 0;
    let mut total_us = 0;
    for i in 0..nh {
        let now = cpu_us(hogs[i], freq);
        shares[i] = now.0 + now.1 - before[i].0 - before[i].1;
        big_us += now.1 - before[i].1;
        total_us += shares[i];
    }
    set_burners(false);
    let (min, max) = shares[..nh].iter().fold((u64::MAX, 0), |(lo, hi), &s| (lo.min(s), hi.max(s)));
    // The hogs (the burners and the old worker) want every core there is.
    let usable = (ncpus as u64).min(nh as u64);
    let busy_pct = total_us * 100 / (wall_us * usable).max(1);
    let fair = min * 10 >= max * 6;
    let (switches, preempts) = (c1[0] - c0[0], (c1[1] - c0[1]) + (c1[2] - c0[2]));
    println!("[client] SCHED {nh} CPU hogs on {ncpus} cores for {} ms: shares {:?} ms, {switches} context switches ({preempts} of them preemptions by slice end or by higher-priority wakeups), {} migrations, {} taken from busy cores",
        wall_us / 1000, shares.iter().take(nh).map(|s| s / 1000).collect::<alloc::vec::Vec<_>>(), c1[3] - c0[3], c1[4] - c0[4]);
    println!("[client] TEST scheduler shares the cores fairly between equal tasks (slowest {min} us, fastest {max} us): {}", verdict(fair));
    println!("[client] TEST scheduler keeps the cores busy (hogs used {busy_pct}% of the {usable} cores they can use): {}", verdict(busy_pct >= 70));
    if ncpus >= 3 {
        let big_pct = big_us * 100 / total_us.max(1);
        // at least its proportional share of the performance cores (they are all there is when the hogs outnumber the cores)
        let big_cores = ncpus as u64 - 2;
        println!("[client] TEST sustained compute prefers the performance cores ({big_pct}% of its time on {big_cores} of {ncpus} cores): {}", verdict(big_pct * ncpus as u64 >= 100 * big_cores));
    }
    if nh > ncpus {
        // more hogs than cores: they take turns, switched by a timer event at the end of the slice (or sooner by
        // work that outranks them), not left to a 10 ms tick: at least one switch per 30 ms on average
        println!("[client] TEST hogs take turns on the cores (the slices are timer events; {switches} switches, {preempts} preemptions in {} ms): {}", wall_us / 1000, verdict(preempts > 0 && switches * 30_000 >= wall_us));
    }

    // ---- 1b. a CPU hog pushed onto an efficiency core goes back to a performance core when one is idle --------------
    // Nothing contends for the hog's core, so its slice never ends and it makes no system call that would let the scheduler
    // place it again: only the scheduler's own check for an idle performance core can move it. Each round oversubscribes
    // the cores, takes the burner that spent most time on efficiency cores, stops all the others, and looks at where it runs.
    if ncpus >= 3 && nb > 1 {
        let (rounds, mut stuck) = (8, 0);
        for _ in 0..rounds {
            set_burners(true);
            sleep(25);
            // the burner that is on an efficiency core right now: the one whose efficiency-core time grew during the last tick
            let mut pick = 0;
            for _ in 0..20 {
                let before: [(u64, u64); 8] = core::array::from_fn(|i| if i < nb { cpu_us(burners[i], freq) } else { (0, 0) });
                sleep(1);
                if let Some(i) = (0..nb).find(|&i| cpu_us(burners[i], freq).0 - before[i].0 >= 6_000) {
                    pick = i;
                    break;
                }
            }
            // SAFETY: same writable flag page; word 1 (offset 8) is inside it.
            unsafe { write_volatile((SPIN_VA + 8) as *mut u64, burners[pick] as u64) };
            // SAFETY: same writable flag page.
            unsafe { write_volatile(SPIN_VA as *mut u64, 3) };
            let (l0, b0) = cpu_us(burners[pick], freq);
            sleep(10);
            let (l1, b1) = cpu_us(burners[pick], freq);
            set_burners(false);
            sleep(12);
            if (l1 - l0) * 2 > (l1 - l0) + (b1 - b0) {
                stuck += 1;
            }
        }
        println!("[client] TEST a CPU hog that was pushed onto an efficiency core goes back to a performance core once one is idle (stuck on an efficiency core for a whole 100 ms window in {stuck} of {rounds} rounds): {}", verdict(stuck == 0));
    }

    // ---- 2. real-time: periodic job, hogs on the same core -----------------------------------------
    sleep(5);
    set_burners(true);
    sleep(15);
    let core = rt_set(2000, 10_000, rt_core);
    println!("[client] SCHED real-time reservation 2 ms / 10 ms on core {core} while {nh} CPU hogs run");
    let jobs = 150u64;
    let mut worst = 0u64;
    let mut sum = 0u64;
    let work = freq / 1000; // 1 ms of work per job
    let mut late_jobs = 0;
    stats(6, 0);
    let mut release = counter();
    for _ in 0..jobs {
        burn_until(counter() + work);
        release = sched_call(SC_RT_WAIT, 0, 0, 0, 0)[0];
        let late = counter().saturating_sub(release) * 1_000_000 / freq;
        worst = worst.max(late);
        sum += late;
        if late > 1000 {
            late_jobs += 1;
        }
    }
    let rs = stats(4, 0);
    let (rjobs, missed) = (rs[0], rs[1]);
    let lat = stats(2, 0);
    rt_clear();
    set_burners(false);
    let _ = release;
    println!("[client] SCHED real-time wakeup lateness: mean {} us, worst {} us over {jobs} periods; kernel saw {} dispatches, worst {} us; {rjobs} jobs, {missed} missed",
        sum / jobs, worst, lat[2], lat[0] * 1_000_000 / freq);
    // The emulator's host thread can be descheduled for a few milliseconds now and then, which no guest
    // scheduler can prevent: allow a couple of such outliers, and judge the rest strictly.
    println!("[client] TEST real-time task meets its deadlines against CPU hogs on its core (core {core}, {missed} missed of {rjobs}; at most 3 tolerated for host jitter): {}", verdict(core >= 0 && missed <= 3));
    println!("[client] TEST real-time task is dispatched within 1 ms of its release, hogs notwithstanding (mean {} us, worst {worst} us, {late_jobs} of {jobs} jobs over 1 ms; at most 5 tolerated): {}", sum / jobs, verdict(late_jobs <= 5 && sum / jobs < 500));

    // ---- 3. real-time: a task that overruns its budget is throttled, the hogs keep the rest -------------------
    set_burners(true);
    sleep(15);
    let core = rt_set(2000, 10_000, rt_core);
    let me = my_tid();
    let (l0, b0) = cpu_us(me, freq);
    let w0 = counter();
    burn_until(w0 + freq / 2); // never says it is done
    let wall = (counter() - w0) * 1_000_000 / freq;
    let (l1, b1) = cpu_us(me, freq);
    let used = l1 + b1 - l0 - b0;
    let rs = stats(4, 0);
    rt_clear();
    set_burners(false);
    let pct = used * 100 / wall.max(1);
    println!("[client] SCHED overrunning real-time task on core {core}: ran {used} us of {wall} us ({pct}%), {} deadlines missed", rs[1]);
    println!("[client] TEST an overrunning real-time task gets its reserved 20% and no more (got {pct}%): {}", verdict((14..=26).contains(&pct) && rs[1] >= 20));

    // ---- 4. admission control -------------------------------------------------------------------------
    let a = rt_set(8000, 10_000, rt_core); // exactly the ceiling
    rt_clear();
    let b = rt_set(8100, 10_000, rt_core);
    let c = rt_set(0, 10_000, rt_core);
    let d = rt_set(11_000, 10_000, rt_core);
    let e = rt_set(1000, 10_000, 9);
    let f = rt_set(1000, 10_000, rt_core); // the failed attempts must not have left a reservation behind
    rt_clear();
    let g = rt_set(8000, 10_000, rt_core);
    rt_clear();
    println!("[client] SCHED admission: 80% -> {a}, 81% -> {b}, runtime 0 -> {c}, runtime > period -> {d}, no such core -> {e}, 10% afterwards -> {f}, 80% again -> {g}");
    println!("[client] TEST real-time admission refuses more than 80% of a core and bad parameters: {}", verdict(a == rt_core as i64 && b == E_FULL && c == E_INVAL && d == E_INVAL && e == E_INVAL && f == rt_core as i64 && g == rt_core as i64));
    let no_cap = sched_call(SC_RT_SET, 0, 1000, 10_000, u64::MAX)[0] as i64;
    println!("[client] TEST a reservation needs the scheduling capability (slot 0 is no such thing: {no_cap}): {}", verdict(no_cap < 0));
    let waited = sched_call(SC_RT_WAIT, 0, 0, 0, 0)[0] as i64;
    println!("[client] TEST waiting for the next period without a reservation is refused ({waited}): {}", verdict(waited == E_INVAL));

    // ---- 5. energy-aware placement: a light task stays on efficiency cores ------------------------------------------
    if ncpus >= 3 {
        sleep(10);
        let (l0, b0) = cpu_us(me, freq);
        let t0 = counter();
        for _ in 0..100 {
            burn_until(counter() + freq / 1000); // about 1 ms of work ...
            sleep(1); // ... then 10 ms of nothing: a 10% task
        }
        let wall = (counter() - t0) * 1_000_000 / freq;
        let (l1, b1) = cpu_us(me, freq);
        let (little, big) = (l1 - l0, b1 - b0);
        let pct = big * 100 / (little + big).max(1);
        println!("[client] SCHED light task ran {} ms on efficiency cores and {} ms on performance cores in {} ms", little / 1000, big / 1000, wall / 1000);
        println!("[client] TEST a light task is kept on the efficiency cores ({pct}% of its time on performance cores): {}", verdict(pct <= 25));
        let u = stats(3, 2);
        let cs = stats(3, 0);
        println!("[client] SCHED measured utilisation: performance core 2 {}/1024, efficiency core 0 {}/1024 (would request {}/1024)", u[0], cs[0], cs[3]);
    }
    // ---- 6. idle cores are not woken by a tick ------------------------------------------------------
    sleep(30);
    let irq0 = stats(7, 0);
    sleep(100);
    let irq1 = stats(7, 0);
    let per: [u64; 4] = core::array::from_fn(|c| irq1[c] - irq0[c]);
    let total: u64 = per.iter().sum();
    let ticking = per.iter().filter(|&&n| n >= 70).count();
    println!("[client] SCHED timer interrupts per core in one second of mostly idle time: {:?} ({} cores online; a 100 Hz tick on every core would be {})", per, ncpus, ncpus * 100);
    if ncpus >= 3 {
        // The CPU hog keeps one core ticking (and moves around); the rest is wakeups for timers (the shell polls, the
        // burners and the memory daemon sleep and wake). With a tick on every core it would be 100 per core, plus those.
        println!("[client] TEST sleeping cores take no periodic tick ({total} timer interrupts in 1 s on {ncpus} cores, {ticking} of them at 70 Hz or more; with a tick on every core: over {}): {}", ncpus * 100, verdict(total < (ncpus as u64 * 100) * 9 / 10));
    }

    if ncpus == 1 {
        inheritance_test(freq);
    }

    let l = stats(2, 1);
    println!("[client] SCHED interactive wake-to-run latency: worst {} us, mean {} us over {} dispatches",
        l[0] * 1_000_000 / freq, l[1] * 1_000_000 / freq / l[2].max(1), l[2]);
}
