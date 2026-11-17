//! Lock-scaling benchmark: independent client/server pairs, each doing IPC round trips
//! against its *own* server. The pairs share nothing, so on a kernel with fine-grained
//! locking N pairs should take about as long as one; under a single kernel lock they serialise.
//! Timing uses the hardware counter (the kernel lets EL0 read it).
use crate::*;
use core::arch::asm;

const SHM_VA: usize = 0x803A_0000;

pub fn counter() -> u64 {
    let v: u64;
    // SAFETY: ISB and the EL0-readable virtual counter (the kernel enables EL0 access); only the output register is written.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v) };
    v
}

pub fn frequency() -> u64 {
    let v: u64;
    // SAFETY: CNTFRQ_EL0 is readable at EL0; only the output register is written.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v) };
    v
}

/// Role 10: echo server for one pair.
pub fn bench_server() -> ! {
    loop {
        let m = recv(0);
        reply([m.w[0], m.w[1] + 1, 0]);
    }
}

/// Role 11: waits for "go N" from the conductor, then performs N round trips and records
/// (elapsed counter ticks, errors, done flag) in its slot of the shared page.
pub fn bench_client(tid: u64) -> ! {
    let k = (tid - 14) as usize;
    if mem_map(2, SHM_VA, true) != 0 {
        println!("[bench] cannot map result page");
        exit(1);
    }
    let slot = (SHM_VA + k * 32) as *mut u64;
    loop {
        let go = recv(1);
        let n = go.w[0];
        let (mut errors, t0) = (0u64, counter());
        for i in 0..n {
            let (st, r) = call(0, [i, i ^ 0x55, 0]);
            if st != 0 || r[0] != i || r[1] != (i ^ 0x55) + 1 {
                errors += 1;
            }
        }
        let t1 = counter();
        // SAFETY: `slot` is this pair's 32-byte slot (k <= 3, so within 128 bytes) of the result page mapped writable at SHM_VA above; the conductor reads it, hence volatile.
        unsafe {
            write_volatile(slot, t1 - t0);
            write_volatile(slot.add(1), errors);
            write_volatile(slot.add(2), go.w[1]); // generation: tells the conductor this run is finished
        }
    }
}

/// Conductor: run 1, 2 and 4 pairs at once (as many as there are cores, since each pair is
/// pinned to its own core) and report aggregate throughput.
pub fn bench_tests() {
    if mem_map(20, SHM_VA, false) != 0 {
        println!("[client] TEST lock-scaling benchmark: cannot map result page: FAIL");
        return;
    }
    let freq = frequency();
    let ncpus = svc(20, 0, 0, 0, 0, 0)[0] as usize;
    let n: u64 = 6000;
    let mut rates = [0u64; 3];
    let mut ran = 0;
    let mut all_ok = true;
    for (gen, &pairs) in [1usize, 2, 4].iter().enumerate() {
        if pairs > ncpus {
            break;
        }
        let generation = gen as u64 + 1;
        let t0 = counter();
        for k in 0..pairs {
            send(16 + k as u64, [n, generation, 0]);
        }
        let mut waited = 0;
        loop {
            // SAFETY: SHM_VA is the result page mapped (checked) by `mem_map(20, ..)` above; k < 4 keeps the read inside the 128 bytes of slots; the clients write it, hence volatile.
            let done = (0..pairs).all(|k| unsafe { read_volatile((SHM_VA + k * 32 + 16) as *const u64) } == generation);
            if done {
                break;
            }
            sleep(1);
            waited += 1;
            if waited > 3000 {
                all_ok = false;
                break;
            }
        }
        let wall = counter() - t0;
        let mut errors = 0;
        for k in 0..pairs {
            // SAFETY: same mapped result page as above (offset k*32 + 8 < 128); volatile because the client tasks wrote it.
            errors += unsafe { read_volatile((SHM_VA + k * 32 + 8) as *const u64) };
        }
        all_ok &= errors == 0;
        // aggregate round trips per second over the wall-clock time of the whole batch
        let rate = pairs as u64 * n * freq / wall.max(1);
        rates[gen] = rate;
        ran = gen + 1;
        println!("[client] BENCH {pairs} independent pair(s) x {n} round trips: {} us wall, {} round trips/s aggregate, {errors} errors",
            wall * 1_000_000 / freq, rate);
    }
    if ran == 3 {
        println!("[client] BENCH scaling: 2 pairs = {}.{:02}x, 4 pairs = {}.{:02}x of one pair's throughput",
            rates[1] * 100 / rates[0].max(1) / 100, rates[1] * 100 / rates[0].max(1) % 100,
            rates[2] * 100 / rates[0].max(1) / 100, rates[2] * 100 / rates[0].max(1) % 100);
    }
    println!("[client] TEST IPC lock-scaling benchmark ran correctly ({ran} configuration(s) on {ncpus} CPU(s), all replies verified): {}", verdict(all_ok && ran > 0));
}
