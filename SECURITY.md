# Metal OS kernel security audit

Date: 2026-11-18. Commit audited: `6406422`. Auditor: Claude (code review).

**Status:** this file lists only open findings. Everything the first audit found at High and Medium (H1, H2, M1 to M7) and L1 has been fixed and removed; see `git log` for the fixes. The Low items L3 to L10 are still open; L2 is fixed and removed. A second audit of `986fe66` follows: its findings N8, N10 and N11 are open; N1 to N7 and N9 are fixed and removed. Line numbers in the first audit refer to the audited commit (`6406422`) and have moved.

## Summary

The kernel is in better shape than most prototypes: the design is capability-based, 15 modules forbid `unsafe`, physical memory access is bounds-checked, and user programs get strict W^X. The first audit found two high-severity problems, seven medium ones and ten low ones; all High and Medium findings and L1 are fixed and removed from this file. Open from that audit:

| # | Severity | Finding |
|---|---|---|
| L3 to L10 | Low | See below |

**How this was done.** Static review only. Nothing was built, booted or exploited, so every finding is a reading of the code, not a demonstrated attack. Line numbers refer to the commit above.

**What was read.**
- In full: `syscall`, `cap`, `task`, `obj`, `spawn`, `ipc`, `ring`, `thread`, `mem`, `swap`, `mmu`, `frame`, `pager`, `trap`, `irq`, `compact`, `devices`, `smmu`, `rand`, `timer`, `platform/qemu_virt`, `boot.S`.
- In part: `sched` (doom, real-time admission, dispatch), `main`, `sync`.
- Not reviewed: `kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, everything under `libs/`, `user/`, `programs/`, and the ROCK 3C port.

**Threat model assumed.** An unprivileged program (role 32) is hostile. Boot-time services and drivers are trusted for their own job, but a compromised driver should not be able to take down or take over the kernel. The host (QEMU, `fw_cfg`) is trusted.

---

## Low

**L3. Holding-limit check is not atomic.** `SHM_CREATE` and `MC_ALLOC` read `held_pages()` under a lock that is dropped before `reserve_contig` runs (`syscall.rs:1180`, `:1246`–`1247`), and `reserve_contig` itself loads, checks, then adds (`spawn.rs:134`–`140`). Threads racing each other can exceed the quota. Hold the lock across the call, or use a compare-exchange loop.

**L4. `CAP_GRANT` to a dying task leaks a node.** The liveness check is made before the target's lock is taken (`syscall.rs:1320`–`1323`). A grant that lands after teardown is wiped by the next occupant without freeing its node. Recheck under the lock.

**L5. The write right is ignored for MMIO objects.** `mem_map` maps device memory read-write whatever the capability says (`syscall.rs:428`–`432`). Refuse the map when `R_WRITE` is missing.

**L6. `LOG` is an unfiltered, unlimited channel to the kernel console.** Any program can write raw bytes to the UART (`syscall.rs:1379`–`1399`): forged `[kernel]` lines, terminal escape sequences, or a flood. Prefix lines with the task id, strip control bytes, and rate-limit.

**L7. Device confinement fails open.** If the SMMU is missing or lacks stage 2, the kernel logs a warning and continues (`smmu.rs:214`–`216`), and drivers still get physical addresses through `CAP_INFO`. On such a machine a driver can direct its device to overwrite any memory. Make this an explicit, visible mode. This is the open question already noted in `docs/PORTING.md`.

**L8. `GETRANDOM` serves a generator documented as not for cryptography.** `rand.rs:1` says so; the ABI says callers may use it for cryptography. It is seeded once at boot; RNDR and virtio-rng are not consulted again. Reseed periodically from RNDR where it exists, and settle what the generator is for.

**L9. Ambient authority and overloaded bits.** `SETTIME` is granted by role number, not by capability (`syscall.rs:200`–`204`). `R_REVOKE` shares a bit with `R_MEM_SWAP`, and `R_POWER` with `R_RT` (`cap.rs:27`–`39`); the kind check keeps this safe today, but one missed kind check turns it into a privilege bug.

**L10. Expensive unprivileged calls under global locks.** `SYSINFO` page 7 scans 65 536 frame entries under the allocator lock; page 13 and `SHM_INFO` run a fixed-point walk of the node table and lock every task. The kernel is not preemptible, so a loop of these delays every core.

---

## Informational

- **KASLR is thin.** About 14 bits with 64 MiB, the boot log prints the kernel's address, and `MC_ALLOC` capabilities carry `R_INFO`, which reveals physical addresses.
- **No speculative-execution mitigations**, and the kernel is mapped in every user address space. This does not matter under QEMU; revisit it per CPU when porting.
- **Semihosting.** The kernel relies on QEMU refusing `hlt #0xf000` from EL0. That is QEMU's default (10.2.1 is installed here) but was not tested.
- **Debug switches** (`noaslr`, `aslr-seed`, `aslr-log`) come from `fw_cfg` and each prints a warning. Fine for a host-trusted setup.
- **Documentation drift:**
  - `docs/ABI.md` says the heap maximum is 4096 pages; the code allows 16 384 (`spawn.rs:40`).
  - `docs/ABI.md` says there are 254 capability nodes; the code has 1024 (`cap.rs:11`).
  - `spawn.rs:7` says the grant right is not passed to children; the code passes it when the mask includes it.

## What is done well

- Every syscall argument that is a user pointer goes through a page-table walk and a bounds-checked physical read; the `try_*` forms keep device pages from panicking the kernel.
- User memory is strictly W^X: `TASK_LOAD` loads a page once, and nothing can later make it writable.
- Lock ranks are checked on every acquisition, so ordering bugs panic at once instead of deadlocking.
- Unattached devices are aborted by default in the SMMU, and drivers get a read-only copy of PCI configuration space, never the bus.
- Guard pages surround user stacks, thread stacks and kernel stacks, with stack probes for large frames.
- Sender ids in IPC are set by the kernel, and stale calls are purged when a task id is reused.
- Arithmetic on user-supplied sizes uses `checked_`/`saturating_` forms nearly everywhere.

---

# Second audit (2026-11-18, commit `986fe66`)

Auditor: Claude (code review). **Status:** N1 to N7 and N9 are fixed and removed; everything else in this section is open.

**How this was done.** Static review only, like the first audit: nothing was built, booted or exploited, so every finding is a reading of the code. The findings that depend on timing between cores are unproven until a test hits them. Line numbers refer to `986fe66`.

**What was read.** Every file under `kernel/src` in full, including the ones the first audit skipped (`kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, `platform/qemu_virt`, all of `sched`, `main` and `sync`); those produced no findings. Not reviewed: `libs/`, `user/`, `programs/` (except to check who clears the session pages and which rights the program manager hands out) and the ROCK 3C port.

**Threat model.** As before: a program (role 32) is hostile. What it can reach depends on its manifest: `threads` (0 to 8) needs no permission; `permission = spawn` gives a spawn capability, an endpoint factory and a budget.

| # | Severity | Finding | Who can trigger it |
|---|---|---|---|
| N8, N10, N11 | Low | See below | |

## Low

**N8. The reverse map cannot hold addresses above 64 GiB.** A movable frame records `tid << 24 | va >> 12` in 32 bits (`kernel/src/mem.rs:49`, `:188`), but position-independent images are placed up to 256 GiB (`PIE_HI`, `kernel/src/task.rs:74`). For about three quarters of the possible bases the address spills into the task number. Those image pages are never compacted or swapped (the check against the page table fails), and `try_evict` and `compact::migrate` lock a different task's slot; if that slot is a thread's, they walk its page-table root (the old N7 made that root stale; it is now 0 for a dead task, but a thread's own slot still has no root of its own). Widen the field.

**N10. Check-then-act in `WAIT` and `TASK_START`.** `wait` decides under the caller's own lock (`kernel/src/spawn.rs:772`), so two threads of one parent can both release the same zombie; if a `SPAWN` takes the slot in between, the second release clears the new program's parent and session. `task_start` can set a just-killed embryo `Ready` after it died (`:753`), and the next dispatch hits the `switch_to` assert (the old N4 crash). Both windows are a few instructions wide.

**N11. A pager with a thread panics the kernel.** `pager::register` checks that the pager has no threads only when it registers (`kernel/src/pager.rs:186`). `SCRATCH` is a `Global`, whose `get` panics at once on an overlapping borrow (`kernel/src/sync.rs:297`); `next`, `start_write` and `open_into` take it under different locks, so two pager threads can overlap.

## Suggested order of work

1. N10 and N11, then L3 to L5 (quick), N8, and the rest of the Low items.

Tests worth adding to `racetest`: a thread creating a thread after the first task died (N4, fixed without a test), a failed `SPAWN` after a program's death (N7, fixed without a test), and the N3 hand-over: a program that ends while another program holds its shared memory must leave the memory charged to that holder (or end it when the holder cannot pay). The N3 fix has a test only for the thread case.
