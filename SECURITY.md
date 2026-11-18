# Metal OS kernel security audit

Date: 2026-11-18. Commit audited: `6406422`. Auditor: Claude (code review).

**Status:** this file lists only open findings. Everything the first audit found at High and Medium (H1, H2, M1 to M7) and L1 has been fixed and removed; see `git log` for the fixes. The Low items L6 to L10 are still open; L2 to L5 are fixed and removed. A second audit of `986fe66` follows: every finding (N1 to N11) is fixed and removed. Line numbers in the first audit refer to the audited commit (`6406422`) and have moved.

## Summary

The kernel is in better shape than most prototypes: the design is capability-based, 15 modules forbid `unsafe`, physical memory access is bounds-checked, and user programs get strict W^X. The first audit found two high-severity problems, seven medium ones and ten low ones; all High and Medium findings and L1 are fixed and removed from this file. Open from that audit:

| # | Severity | Finding |
|---|---|---|
| L6 to L10 | Low | See below |

**How this was done.** Static review only. Nothing was built, booted or exploited, so every finding is a reading of the code, not a demonstrated attack. Line numbers refer to the commit above.

**What was read.**
- In full: `syscall`, `cap`, `task`, `obj`, `spawn`, `ipc`, `ring`, `thread`, `mem`, `swap`, `mmu`, `frame`, `pager`, `trap`, `irq`, `compact`, `devices`, `smmu`, `rand`, `timer`, `platform/qemu_virt`, `boot.S`.
- In part: `sched` (doom, real-time admission, dispatch), `main`, `sync`.
- Not reviewed: `kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, everything under `libs/`, `user/`, `programs/`, and the ROCK 3C port.

**Threat model assumed.** An unprivileged program (role 32) is hostile. Boot-time services and drivers are trusted for their own job, but a compromised driver should not be able to take down or take over the kernel. The host (QEMU, `fw_cfg`) is trusted.

---

## Low

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

Auditor: Claude (code review). **Status:** every finding of this audit (N1 to N11) is fixed and removed from this file; see `git log` for each fix.

**How this was done.** Static review only, like the first audit: nothing was built, booted or exploited, so every finding is a reading of the code. The findings that depend on timing between cores are unproven until a test hits them. Line numbers refer to `986fe66`.

**What was read.** Every file under `kernel/src` in full, including the ones the first audit skipped (`kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, `platform/qemu_virt`, all of `sched`, `main` and `sync`); those produced no findings. Not reviewed: `libs/`, `user/`, `programs/` (except to check who clears the session pages and which rights the program manager hands out) and the ROCK 3C port.

**Threat model.** As before: a program (role 32) is hostile. What it can reach depends on its manifest: `threads` (0 to 8) needs no permission; `permission = spawn` gives a spawn capability, an endpoint factory and a budget.

## Suggested order of work

1. L6 to L10 (these need decisions on policy).

Tests worth adding to `racetest`: a thread creating a thread after the first task died (N4, fixed without a test), a failed `SPAWN` after a program's death (N7, fixed without a test), and the N3 hand-over: a program that ends while another program holds its shared memory must leave the memory charged to that holder (or end it when the holder cannot pay). The N3 fix has a test only for the thread case. L4 (a `CAP_GRANT` to a dying task) and L5 (a device window without the write right) were fixed without tests: L4 needs a grant landing at the moment of a death, and L5 needs a capability to a device window with the write right dropped, which no user program can make (the drivers' window capabilities have no grant right).
