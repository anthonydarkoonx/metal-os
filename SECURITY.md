# Metal OS kernel security audit

Date: 2026-11-18. Commit audited: `6406422`. Auditor: Claude (code review).

**Status:** this file lists only open findings. Everything the first audit found at High and Medium (H1, H2, M1 to M7) and L1 has been fixed and removed; see `git log` for the fixes. The Low items L2 to L10 are still open. A second audit of `986fe66` follows: its findings N3, N5, N6 and N8 to N11 are open; N1, N2, N4 and N7 are fixed and removed. Line numbers in the first audit refer to the audited commit (`6406422`) and have moved.

## Summary

The kernel is in better shape than most prototypes: the design is capability-based, 15 modules forbid `unsafe`, physical memory access is bounds-checked, and user programs get strict W^X. The first audit found two high-severity problems, seven medium ones and ten low ones; all High and Medium findings and L1 are fixed and removed from this file. Open from that audit:

| # | Severity | Finding |
|---|---|---|
| L2 to L10 | Low | See below |

**How this was done.** Static review only. Nothing was built, booted or exploited, so every finding is a reading of the code, not a demonstrated attack. Line numbers refer to the commit above.

**What was read.**
- In full: `syscall`, `cap`, `task`, `obj`, `spawn`, `ipc`, `ring`, `thread`, `mem`, `swap`, `mmu`, `frame`, `pager`, `trap`, `irq`, `compact`, `devices`, `smmu`, `rand`, `timer`, `platform/qemu_virt`, `boot.S`.
- In part: `sched` (doom, real-time admission, dispatch), `main`, `sync`.
- Not reviewed: `kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, everything under `libs/`, `user/`, `programs/`, and the ROCK 3C port.

**Threat model assumed.** An unprivileged program (role 32) is hostile. Boot-time services and drivers are trusted for their own job, but a compromised driver should not be able to take down or take over the kernel. The host (QEMU, `fw_cfg`) is trusted.

---

## Low

**L2. `RESTORE_OF` can resurrect revoked authority.** The log stores the parent's node id and only checks that the id is alive (`syscall.rs:986`). If the granter revoked or died and the id was reused, the capability comes back under an unrelated node. Needs the admin to act; fixed by node generations, which now exist for capabilities but not yet for the log's parent id (`Revoked.parent`): store the generation there too.

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

Auditor: Claude (code review). **Status:** N1, N2, N4 and N7 are fixed and removed; everything else in this section is open.

**How this was done.** Static review only, like the first audit: nothing was built, booted or exploited, so every finding is a reading of the code. The findings that depend on timing between cores are unproven until a test hits them. Line numbers refer to `986fe66`.

**What was read.** Every file under `kernel/src` in full, including the ones the first audit skipped (`kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, `platform/qemu_virt`, all of `sched`, `main` and `sync`); those produced no findings. Not reviewed: `libs/`, `user/`, `programs/` (except to check who clears the session pages and which rights the program manager hands out) and the ROCK 3C port.

**Threat model.** As before: a program (role 32) is hostile. What it can reach depends on its manifest: `threads` (0 to 8) needs no permission; `permission = spawn` gives a spawn capability, an endpoint factory and a budget.

| # | Severity | Finding | Who can trigger it |
|---|---|---|---|
| N3 | Medium | Shared memory orphaned by a thread's exit is charged to nobody | A program with 1 thread |
| N5 | Medium | File and network session pages go to the next program uncleared | Any program with `files` or `network` |
| N6 | Medium | `SPAWN` racing the first task's exit leaves an embryo nobody owns | A program with `spawn` and 1 thread; SMP only |
| N8–N11 | Low | See below | |

## Medium

### N3. Shared memory orphaned by a thread's exit is charged to nobody

When the creator of shared memory ends while someone holds a copy, the object becomes an orphan: its frames go to the kernel and the creator's charge is returned at once (`kernel/src/task.rs:658`–`664`, `kernel/src/obj.rs:255`–`269`). The creator is a task, so a thread counts.

A thread creates shared memory, grants a copy to its own first task (allowed: same program) and exits. The program keeps the memory, mapped, and is charged nothing. It repeats with a new thread. One capability table holds about 30 such objects, each as large as the program's heap limit (up to 4096 pages), and each thread adds another table. The frames are pinned, so they cannot be swapped or compacted. On a 64 MiB machine this uses up all memory.

The same steps get around the per-program limits on memory objects and capability nodes: an orphan has no owner in the object table (`o.owner = NO_OWNER`), and the nodes of the copies were made by `CAP_GRANT` from a thread that is gone, so they count for nobody.

A parent and a child program can do the same without threads (the child grants to its parent and exits).

**Fix.** Charge an orphan to its holders (split, or to the first holder), or orphan only when the whole program ends and free what a thread created when the thread ends, as the ABI already says ("what a thread creates is its own: it goes when the thread ends").

### N5. Session pages go to the next program uncleared

The eight file session pages and the eight network session pages are allocated once at boot (`kernel/src/devices.rs:371`, `:408`). A program's session is a number into that pool (`claim_fs_page`, `claim_net_page`, `kernel/src/spawn.rs:402`–`425`), given back when the program ends and handed to the next program, lowest number first. Nothing clears the page: not the kernel, not the VFS server (it only empties the descriptor table when the generation changes), not `netd` (it only closes the old handles).

So a program reads what its predecessor in that session last sent or received through the page: up to 3840 bytes of file content and paths, or of network data. A program confined to its own directory sees data from outside it.

**Fix.** Zero the page in `spawn` when the session is claimed.

### N6. `SPAWN` racing the first task's exit leaves an embryo nobody owns

While `spawn` builds a child, the slot's state is still `Free`, but `PARENT[tid]` is already set (`kernel/src/spawn.rs:559`); the state becomes `Embryo` at the end (`:682`).

If the program's first task exits or faults while one of its threads is in `spawn`, `on_death` finds the slot by its parent, clears the parent, and does nothing else because the state is `Free` (`:820`–`824`). The thread then finishes the call. The result is an embryo with no parent: nobody can load, start or kill it, and nothing collects it. The exit holds every task lock, so the thread waits for it inside `spawn`; the window is most of the call.

Each time, a task slot is lost for good (there are 37), with about 20 frames and, if asked for, a file and a network session (8 each, 6 usable by programs). The program dies in the attempt, so the rate is limited by how often it is started.

**Fix.** Give a slot under construction a state of its own, or have `spawn` check, before it sets `Embryo`, that the parent is alive and still the parent, and undo otherwise.

## Low

**N8. The reverse map cannot hold addresses above 64 GiB.** A movable frame records `tid << 24 | va >> 12` in 32 bits (`kernel/src/mem.rs:49`, `:188`), but position-independent images are placed up to 256 GiB (`PIE_HI`, `kernel/src/task.rs:74`). For about three quarters of the possible bases the address spills into the task number. Those image pages are never compacted or swapped (the check against the page table fails), and `try_evict` and `compact::migrate` lock a different task's slot; if that slot is a thread's, they walk its page-table root (the old N7 made that root stale; it is now 0 for a dead task, but a thread's own slot still has no root of its own). Widen the field.

**N9. `RESTORE_OF` restores without rechecking.** `restore_of` checks that the target is alive before taking its lock (`kernel/src/syscall.rs:998`, `:1016`), installs the capability, drops the lock, and `remap` maps without checking `node_alive` (`:967`–`990`). A revoke of the parent in between leaves a mapping of memory the program no longer holds (the same race as the old N1, which node generations closed for `SPAWN`, `THREAD_CREATE`, `CAP_GRANT` and `MEM_MAP`; this path still checks the bare node number); a target that died in between gets a capability whose node leaks. Needs the admin to act.

**N10. Check-then-act in `WAIT` and `TASK_START`.** `wait` decides under the caller's own lock (`kernel/src/spawn.rs:772`), so two threads of one parent can both release the same zombie; if a `SPAWN` takes the slot in between, the second release clears the new program's parent and session. `task_start` can set a just-killed embryo `Ready` after it died (`:753`), and the next dispatch hits the `switch_to` assert (the old N4 crash). Both windows are a few instructions wide.

**N11. A pager with a thread panics the kernel.** `pager::register` checks that the pager has no threads only when it registers (`kernel/src/pager.rs:186`). `SCRATCH` is a `Global`, whose `get` panics at once on an overlapping borrow (`kernel/src/sync.rs:297`); `next`, `start_write` and `open_into` take it under different locks, so two pager threads can overlap.

## Suggested order of work

1. N3 and N5: no race needed, any program with the right manifest can do them today.
2. N6, then the Low items.

Tests worth adding to `racetest`: a thread creating a thread after the first task died (N4, fixed without a test), a failed `SPAWN` after a program's death (N7, fixed without a test), a thread orphaning memory in a loop while the parent watches `QUOTA` (N3), a program reading its session page before its first call (N5).
