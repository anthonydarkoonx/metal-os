# Metal OS kernel security audit

Date: 2026-11-18. Commit audited: `6406422`. Auditor: Claude (code review).

**Status:** H1 and H2 were fixed in `8ab56dc`. Everything else is still open. The line numbers below refer to the audited commit and have moved in the files the fix touched (`syscall.rs`, `spawn.rs`, `swap.rs`).

## Summary

The kernel is in better shape than most prototypes: the design is capability-based, 15 modules forbid `unsafe`, physical memory access is bounds-checked, and user programs get strict W^X. The audit found two high-severity problems that an unprivileged program could reach on a multi-core machine, both of which end with a program holding a writable mapping of physical memory it no longer owns. Both are now fixed (see below).

| # | Severity | Finding | Who can trigger it |
|---|---|---|---|
| H1 | High (fixed in `8ab56dc`) | Capability checks race with revocation; a mapping can outlive freed shared memory | Any two cooperating tasks, SMP only |
| H2 | High (fixed in `8ab56dc`) | `HEAP_RELEASE` frees frames before flushing the TLB | Any program with a thread, SMP only |
| M1 | Medium | Run-time memory objects can be freed through a derived capability | Holder of `MemCtl` with `R_MEM_ALLOC` |
| M2 | Medium | `CAP_GRANT` puts capabilities into any task's table without its consent | Any program |
| M3 | Medium | `WAIT_WORD`/`WAKE_WORD` on a device mapping panics the kernel | Any driver |
| M4 | Medium | FP/SIMD registers are enabled for EL0 but never context-switched | Any program |
| M5 | Medium | All RAM is writable and executable at EL1; no PAN, no WXN | Hardening gap |
| M6 | Medium | The pager sees and supplies other programs' pages in the clear | The pager program |
| M7 | Medium | Global kernel tables have no per-program quotas | Any program (some need threads or spawn) |
| L1–L10 | Low | See below | |

**How this was done.** Static review only. Nothing was built, booted or exploited, so every finding is a reading of the code, not a demonstrated attack. Line numbers refer to the commit above.

**What was read.**
- In full: `syscall`, `cap`, `task`, `obj`, `spawn`, `ipc`, `ring`, `thread`, `mem`, `swap`, `mmu`, `frame`, `pager`, `trap`, `irq`, `compact`, `devices`, `smmu`, `rand`, `timer`, `platform/qemu_virt`, `boot.S`.
- In part: `sched` (doom, real-time admission, dispatch), `main`, `sync`.
- Not reviewed: `kaslr`, `virtio_rng`, `pci`, `smp`, `gic`, `uart`, `cache`, `hal`, everything under `libs/`, `user/`, `programs/`, and the ROCK 3C port.

**Threat model assumed.** An unprivileged program (role 32) is hostile. Boot-time services and drivers are trusted for their own job, but a compromised driver should not be able to take down or take over the kernel. The host (QEMU, `fw_cfg`) is trusted.

---

## High

### H1. Capability checks race with revocation (fixed in `8ab56dc`)

**Fixed:** `MEM_MAP`, `CAP_GRANT`, `SPAWN` delegation and `THREAD_CREATE` now recheck `node_alive` under a task lock before they install or mint anything. `cap_revoke` frees shared and dynamic memory under the same world lock as the revoke. Not done: the generation counter on nodes, so L2 stays open. `make fuzz` passes, but no test races a map against a revoke yet.

The original finding follows.

`syscall::lookup` copies the capability out under the task lock and releases the lock before the caller acts on it (`kernel/src/syscall.rs:121`). The comment calls a late revoke harmless. That is true for one IPC message, but not for calls that create lasting state:

- **`MEM_MAP`** (`syscall.rs:396` lookup, `:415` lock, `:433` map). A revoke that completes between the lookup and the lock clears the capability and unmaps everything, then `mem_map` maps the object anyway and records the mapping under a dead node. Nothing rechecks `node_alive`.
- **`CAP_GRANT`** (`syscall.rs:1312` lookup, `:1339` mint). The new node is minted under a parent that may already be freed, so the copy survives the revoke.
- **`SPAWN` delegation** (`spawn.rs:475`–`493` read, `:629` mint) and **`THREAD_CREATE`** (`spawn.rs:855`–`865`, `:901`) have the same gap.

The worst case is shared memory. `cap_revoke` drops the world lock after `revoke_subtree` and only then calls `shm_destroy` (`syscall.rs:1362`–`1363`), which frees the frames and the frame list. A racing `MEM_MAP` then:

1. reads frame numbers from the freed list page (`obj::shm_frame`, `obj.rs:206`), and
2. maps those freed frames read-write into the caller.

The caller keeps a writable view of frames the allocator will hand to someone else, including page tables. If the freed list page is reallocated to the attacker first, the frame numbers are attacker-chosen.

The window is short but retryable: the revoker only has to take the mapper's task lock in the gap, and the attacker controls both sides. One thread creates and revokes, another maps; or two programs cooperate, since `CAP_GRANT` needs no channel (see M2). It also hits the documented compositor pattern, where a server destroys memory a client still holds. It needs two cores; `make run` defaults to `SMP=4`.

**Fix.**
- Recheck `node_alive(c.node)` after taking the task lock that the install happens under. `revoke_subtree` holds every task lock while it frees nodes, so a check under any task lock is race-free.
- Move `shm_destroy` (and the frees in `MC_FREE` and `cap_revoke`) under the same world lock as the revoke.
- Give nodes a generation counter and store it in `Cap`, `MapRec` and the revoke log, so a reused node id can never match a stale reference (this also fixes L2).

### H2. `HEAP_RELEASE` frees frames before flushing the TLB (fixed in `8ab56dc`)

**Fixed:** the new `swap::release_pages` unmaps a batch of pages, flushes the TLB, and only then frees the frames. `heap_release`, `free_thread_stack` and the failure path of `make_thread_stack` use it. No test yet touches heap pages while a sibling thread releases them.

The original finding follows.

`heap_release` (`syscall.rs:1072`–`1086`) calls `swap::release_page` for each page, which unmaps and immediately frees the frame (`swap.rs:327`–`330`). The TLB flush comes once, after the loop.

A second thread of the same program on another core still has TLB entries for those pages. Freed movable frames go back to the top of RAM, exactly where the next `alloc_movable` looks, so another program's fresh heap or stack page is likely to land on one. Until the flush, the sibling thread can read and write it.

The attacker controls the window: `count` can be the whole heap (up to 16 384 pages), and every iteration takes the allocator lock.

The same order appears in `spawn::free_thread_stack` (`spawn.rs:983`–`990`) and the failure path of `make_thread_stack` (`spawn.rs:971`–`974`).

**Fix.** Unmap all pages, flush, then free. `swap::try_evict` already does it in that order (`swap.rs:145`–`163`).

---

## Medium

### M1. Run-time memory objects can be freed through a derived capability

An `MC_ALLOC` object should live as long as its creator's capability. Three paths free it through a copy instead. The copy comes from `THREAD_CREATE`, which duplicates capabilities without requiring the grant right (`spawn.rs:861`).

- **`CAP_REVOKE` with flag 1 from a thread** (`syscall.rs:1368`–`1373`). Only the thread's subtree is revoked, then all frames are freed and the object released. The leader's capability and mapping stay live, pointing at freed frames.
- **Thread exit** (`task.rs:671`). `World::reset` releases the object-table entry for any non-shared dynamic object the dying task holds, copy or not. The id is reused by the next `SHM_CREATE` or `MC_ALLOC`, and the leader's old capability (map, write, info) now names someone else's memory.
- **`MC_FREE` on a shared-memory capability** (`syscall.rs:1212`–`1231`). It accepts any dynamic object, with no rights or root check. For a `Shared` object `o.pa` is 0, so `mem::owner_of(0)` indexes far out of bounds (`mem.rs:201`) and the kernel panics.

All three need a `MemCtl` capability with `R_MEM_ALLOC`. Today only the conductor, the fuzzer and test programs hold one, which is why this is Medium and not High.

**Fix.** Record the root node for `Dma` objects as is done for `Shared`. Free frames and release the entry only when the root goes; a copy just goes. Make `MC_FREE` reject `Shared` objects.

### M2. `CAP_GRANT` needs no consent from the receiver

`cap_grant` (`syscall.rs:1311`–`1347`) lets any holder of a grantable capability put a copy into any live task, in the first free slot or in a slot it names. Every program has a grantable capability, because `SHM_CREATE` is unprivileged and returns one with `R_GRANT`.

- **Slot exhaustion.** One attacker can fill all 31 free slots of a victim (the grant quota is 40). The victim's `EP_CREATE`, `SHM_CREATE` and legitimate grants then fail. Targets include the VFS server, `netd` and the program manager.
- **Slot squatting.** Programs find capabilities by convention. A program started without a console endpoint has slot 7 empty and falls back to `LOG` (`programs/src/lib.rs:306`–`311`). An attacker who grants its own endpoint into slot 7 receives that program's output and feeds its input. Embryos can be targeted before they start.

**Fix.** Restrict the target to the caller's own program, its children and its parent; or require the receiver to opt in (a receive slot it opens). Keep the exact-slot form for the parent only.

### M3. `WAIT_WORD`/`WAKE_WORD` on a device mapping panics the kernel

`thread::wait` and `thread::wake` translate the address and call `key_of` before any RAM check (`thread.rs:45`–`46`, `:104`–`105`). `key_of` calls `mem::is_movable(pa)`, which indexes the frame table with `(pa - RAM_BASE) / PAGE` (`mem.rs:196`–`198`). For an MMIO page (the virtio BARs sit below `RAM_BASE` on QEMU) the subtraction wraps and the bounds check panics.

Any driver can do this with one system call. That defeats the point of running drivers as restartable, fault-isolated tasks.

**Fix.** Return `E_INVAL` in `key_of`'s callers when `phys::is_ram_page` fails, and make `is_movable`, `owner_of`, `set_owner` and `free` range-check instead of indexing blindly.

### M4. FP/SIMD state is not context-switched

`CPACR_EL1.FPEN` is set to 3 on every core (`boot.S:17`–`18`, `:174`–`175`), so EL0 may use `V0`–`V31`, `FPCR` and `FPSR`. `TrapFrame` holds only the general registers (`trap.rs:12`–`17`), and nothing else saves the rest. `TPIDR_EL0` is not saved either.

- Two programs that are otherwise isolated can talk through the vector registers.
- Any program that uses FP or NEON (a hard-float ELF, or assembly crypto) leaks its register contents to the next task on that core and gets its own state corrupted.

The in-tree programs are built soft-float, so they are not affected today; the loader accepts any AArch64 ELF.

**Fix.** Either trap EL0 FP use (`FPEN = 0b01`) and kill or lazily switch, or save and restore the FP state and `TPIDR_EL0` in `dispatch`.

### M5. No write-xor-execute in the kernel; no PAN

The RAM identity map is built with `ATTR_NORMAL | SH_INNER | AF | UXN` (`mmu.rs:78`, `:86`, `:102`): writable and executable at EL1 for every frame, including kernel text and every user page. `SCTLR_EL1.WXN` and `PSTATE.PAN` are never set (`mmu.rs:139`–`145`).

On its own this is not exploitable. Combined with any kernel write primitive (H1, H2, M1) it removes the remaining obstacles: kernel text can be patched, and user-controlled frames are executable at EL1 through their identity-mapped address.

**Fix.** Map kernel text read-only and executable, everything else `PXN`. Set PAN: the kernel never dereferences user virtual addresses, so it costs nothing.

### M6. The pager is trusted with every program's memory

Write-back hands the pager the plaintext page (`pager.rs:204`–`210`). Read-in installs whatever bytes the pager returns, with the page's old permissions, including executable ones (`pager.rs:311`–`326`).

A buggy or hostile pager can read any eligible program's swapped pages and replace their contents or code. The module comment says the kernel "never trusts the pager with more than it asks of it"; in practice it trusts it with confidentiality and integrity.

**Fix.** Encrypt and authenticate each page in the kernel with a per-boot key before it leaves (AEAD keyed by slot and a counter), and never write back executable pages.

### M7. Global tables have no per-program quotas

Several fixed-size tables are shared by everyone, and one program can use them up:

| Resource | Size | How it is exhausted |
|---|---|---|
| Run-time memory objects | 192 | `SHM_CREATE` until slots run out; threads and children each have their own 31 slots |
| Run-time endpoints | 16 | 4 per task, and each thread is a task (`ipc.rs:95`) |
| Capability nodes | 1024 | `THREAD_CREATE` with a full mask copies 31 capabilities per thread; only `CAP_GRANT` has a quota |
| Endpoint queues | 8 messages | `IPC_SEND` never blocks; one client can keep a shared server's queue full (`ipc.rs:256`) |
| File and network sessions | 8 each | Any holder of the spawn right |

Node exhaustion has a sharper edge: restarting a boot service calls `install`, which does `expect("capability nodes exhausted")` (`task.rs:282`). Other cores can take nodes between the old instance's teardown and the rebuild, so a service crash at the wrong moment becomes a kernel panic.

**Fix.** Charge nodes, objects and endpoints to the program (address space), not the task. Make `build` fail the restart instead of panicking. Give endpoints a per-sender queue share.

---

## Low

**L1. `SYSINFO` exposes other tasks' authority.** Pages 10, 11, 13 and 14 return any task's capability slots, object ids, rights, node ids and revoke log to any caller (`syscall.rs:647`–`756`). The comment says it reveals nothing a task could not observe anyway; that does not hold for these pages. It also tells an attacker which slots are free for M2. Gate them behind the `Admin` capability.

**L2. `RESTORE_OF` can resurrect revoked authority.** The log stores the parent's node id and only checks that the id is alive (`syscall.rs:986`). If the granter revoked or died and the id was reused, the capability comes back under an unrelated node. Needs the admin to act; fixed by node generations (H1).

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

- Every syscall argument that is a user pointer goes through a page-table walk and a bounds-checked physical read; the `try_*` forms keep device pages from panicking the kernel (M3 is the one miss).
- User memory is strictly W^X: `TASK_LOAD` loads a page once, and nothing can later make it writable.
- Lock ranks are checked on every acquisition, so ordering bugs panic at once instead of deadlocking.
- Unattached devices are aborted by default in the SMMU, and drivers get a read-only copy of PCI configuration space, never the bus.
- Guard pages surround user stacks, thread stacks and kernel stacks, with stack probes for large frames.
- Sender ids in IPC are set by the kernel, and stale calls are purged when a task id is reused.
- Arithmetic on user-supplied sizes uses `checked_`/`saturating_` forms nearly everywhere.

## Suggested order of work

1. ~~H1 and H2~~ (done in `8ab56dc`; node generations from H1 are still open and would also close L2).
2. M1 and M3: lifetime rules for dynamic objects and the range checks in `mem.rs`.
3. M2 and L1: decide who may grant to whom, then hide the introspection pages.
4. M4 and M5: FP trapping or switching, kernel page permissions, PAN.
5. M7, then the Low items.

Tests worth adding to `make fuzz`: a two-thread program that maps while its sibling revokes, one that touches heap pages while its sibling releases them, and a driver-role task calling `WAIT_WORD` on its register window.
