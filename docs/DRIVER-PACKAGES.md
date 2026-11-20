# Driver packages: swapping a driver without rebuilding the kernel (plan and threat model, 2026-11-16)

Status: **plan only**; nothing here is built yet. Stages 1, 2 and 3a (platform layer, device registry, capability manifest in `kernel/src/devices.rs`) are done.

## Goal
Replace or upgrade the driver of a device (display, input, network; later others) by installing a signed package, with no rebuild of the kernel image and
no reboot of the rest of the system. A driver stays an ordinary confined program: its own address space, only the capabilities its device needs, its
device's DMA held inside the IOMMU.

Not goals: loading code into the kernel (this system has no loadable kernel modules and should not get them); replacing the block driver and the storage
servers at run time (they must be there before any package can be read, see below).

## What exists now (facts from the code)
* Drivers are roles of the single `user/` image (`user/src/main.rs` `_start`), embedded in the kernel with `include_bytes!` (`task.rs`). The kernel starts
  them and restarts them when they die.
* `kernel/src/devices.rs` holds, per driver: the PCI id it matches, its role, the tasks that exist because of it, the capabilities it starts with (`grants`)
  and the memory its device may reach (`dma`). Boot-time capability set-up and the IOMMU attach (`smmu::attach`) are driven by that table.
* A program started at run time (`SPAWN`, `docs/ABI.md`) gets up to eight delegated capabilities, can be signed and checked by `metallium-map`, is not
  restarted by the kernel (its parent supervises), and has role `ROLE_PROGRAM` (`set_dynamic_info` can give another role).
* Kernel rules keyed on a task's role: IOMMU fault condemns the role's tasks (`smmu.rs`), the stream is torn down at the task's death (`task.rs`),
  `restrict_boot_task`, heap quotas.

## Design

### Two kinds of driver
1. **Boot drivers**, embedded in the kernel image: the block driver and the storage servers (a package is read *through* them), plus a known-good
   copy of every other driver. Started by the kernel as now.
2. **Package drivers**, in `/system/usr/drivers/<id>.map` (or `/data`): display, input, network and later others. Started by the **driver manager**.
   When one is installed, valid and trusted it replaces the embedded copy for its device; if it fails (crashes repeatedly, refused signature,
   rolled-back version) the embedded copy runs. The system never ends up without a working driver for a device it booted with.

### The driver manager (`drvmgr`)
A boot-time task (not `progmgr`: it holds device authority and `progmgr` runs untrusted programs). It reads driver packages through the VFS, verifies
them (below), asks the kernel to start a driver for a device, supervises it (restart with back-off, then fall back to the embedded copy), and can stop
one to replace it. It holds a new **driver-spawn capability** (rights `R_DRVSPAWN`) and nothing else that reaches devices.

### Principle: the manifest asks, the kernel grants
A package's manifest only *names* a device (`device = pci:1af4:1050`, class, minimum ABI version, heap and thread limits). It does **not** list kernel
objects. What the driver actually receives (its capabilities, their slots, the DMA objects of its IOMMU stream) is what the kernel's registry says for
that device (`DRIVERS[..].grants` / `.dma`), and only for a device the kernel has bound. So a package cannot ask for memory it should not have: the
worst it can claim is a device that exists.
* **Level 1 (this plan):** a new driver for a device type the kernel already knows (a new virtio-gpu driver, say). Slot layout is a per-class contract
  documented in `docs/ABI.md`.
* **Level 2 (later, separate decision):** a device type the kernel has no entry for. The kernel would have to expose a matched device generically (its
  BARs, an interrupt, a DMA pool of a size the manifest asks for, bounded by policy). More powerful, more attack surface: not in scope now, but the
  manifest format reserves room for it.

### New kernel surface (small, all behind `R_DRVSPAWN`)
1. `DRV_SPAWN(device class, package image ...)`: like `SPAWN`+`TASK_LOAD`+`TASK_START`, but the kernel itself delegates the registry's capabilities for
   that device and **attaches the device's IOMMU stream to the registry's DMA objects** (the one step an ordinary spawn cannot do). The new task gets
   the device's role, so the existing role-keyed rules (IOMMU fault, stream teardown) apply unchanged.
2. `DRV_STOP(class)`: condemn the driver, tear the IOMMU stream down *first* (so in-flight DMA aborts), then free its memory and capabilities; the device is
   then reset by the next driver's start-up as it is today after a crash.
3. The registry must know whether a class currently has a package driver or the embedded one (so a stopped embedded driver is not restarted by the
   kernel while the manager is replacing it).

## Threat model

Assets: kernel and every other program's memory; the data partition; the display/keyboard (spying, spoofing input); availability of the device.
Attacker: (a) a malicious or buggy driver package, signed or not; (b) someone who can write the data partition (it is writable: packages there can be
swapped); (c) a stolen or misused signing key; (d) a compromised driver once running.

| Threat | Mitigation |
|---|---|
| Unsigned or tampered package | `metallium-map` checks (CRC, Ed25519) at install **and at every start**, as for programs. |
| A package signed with an ordinary *program* key | **Separate trust store for drivers** (`/system/etc/driver-keys`). A key trusted for programs is not trusted for drivers. |
| Package asks for objects it should not have | Not possible: the manifest cannot name objects; the kernel grants the registry's set for a bound device only. |
| Compromised driver reads or writes other memory by DMA | IOMMU confinement to the registry's DMA objects (as now). **Without an IOMMU (ROCK 3C)** a DMA-capable device reaches everything: see decision 2. |
| Compromised driver attacks the kernel through syscalls | Same capabilities and the same syscall surface as the boot drivers; the fuzzer (`make fuzz`) already runs hostile syscalls with mixed capabilities. Add a driver-role run. |
| Driver manager compromised | It can load only validly signed drivers and cannot name memory (no raw attach). It cannot read any program's memory. It *can* stop a driver (denial of service), as a supervisor must. |
| Rollback to an old vulnerable driver | Policy file holds a minimum version per device; the manager refuses lower. (The data partition could still be rolled back if the policy lives there: the policy stays in the read-only system image.) |
| A bad update leaves no display / no input | Embedded known-good copy; fallback after N crashes or a failed start. |
| Swap while DMA is in flight | `DRV_STOP` tears the IOMMU stream down first; only then are pages reused. |
| Resource exhaustion by a driver | Heap and thread limits from the manifest, capped by the kernel (the table's role limits). |
| Time-of-check/time-of-use on the data partition | The manager reads the package into its own memory, verifies that copy, and spawns from that copy. |

Residual risks to state plainly: a malicious *signed* driver for a device is trusted with that device (it can lie about keystrokes or the screen, as any
driver can); on a machine with no IOMMU a signed DMA-capable driver has full memory access.

## Steps
Each step boots, passes the existing QEMU checks, and gets a line in `docs/BOARD-SYNC.md`.

| Step | Work | Notes / acceptance |
|---|---|---|
| **3b-i** (done 2026-11-16, see below) | New crate for the driver runtime (system-call wrappers, `println!`, the handle/mapping layer, virtio, DMA cache ops, ring): what `blk.rs`, `gpu.rs`, `kbd.rs`, `netdrv.rs` use via `use crate::*` today. Each driver becomes its own binary built against it. | Still embedded, still started by the kernel. Same boot log. `gpu.rs` also uses `crate::info`: that coupling must be cut or the desktop-info task stays separate. |
| **3b-ii** | The kernel starts each driver from its own embedded image (a table entry names the image) instead of from the shared `USER_IMG` with a role number. | Same behaviour; removes the single-image assumption. |
| **3b-iii-a** | `R_DRVSPAWN`, `DRV_SPAWN`, `DRV_STOP`, registry state "package vs embedded", IOMMU attach at spawn. `drvmgr` with signature check, driver-keys, version floor, supervision and fallback. | New tests: tampered package refused; package signed with a program key refused; version below the floor refused; a swapped display driver while the desktop runs; fallback after repeated crashes; a driver that tries DMA outside its objects is condemned; driver-role run of `make fuzz`. |
| **3b-iii-b** | Packaging tool (`tools/mkmap.py` driver kind), the driver manifest keys, `docs/ABI.md` per-class slot contracts. | |
| **3b-iv** | Board: build its drivers as packages from `rock 3c test/`. | Board work; the board has no IOMMU, see decision 2. |

## Decisions (made 2026-11-16, by the user, all four as recommended)
1. **Level 1 only** (a new driver for a device type the kernel already knows). The manifest reserves room for Level 2.
2. **No-IOMMU machines: run-time driver loading is refused** until a bounce-buffer design exists. Embedded drivers keep working there.
3. **A separate driver-signing key** (`/system/etc/driver-keys`); `allow_unsigned` never applies to drivers, even in developer mode.
4. **The block driver and the storage servers are never replaced at run time** (they stay embedded; changing them means a new image).

## Progress

**3b-i (2026-11-16): done.** New crate `libs/metallium-driver-rt` holds the system-call layer, `println!`, the mapping layer, virtio, DMA cache maintenance, the driver
protocols (`proto::{blk, gpu, input, net}`) and the network ring layout (`netring`). The four drivers (`blk.rs`, `gpu.rs`, `kbd.rs`, `netdrv.rs`) now `use`
only that crate: no line of them names anything else of the user image, so each can become its own binary (3b-ii). The user image re-exports the crate
(`pub use metallium_driver_rt::*` in `main.rs`) so every other file, and the board's overlay drivers, compile as before. The crate compiles on the host (the
four AArch64 instructions have host stand-ins), so `cargo test --workspace` still builds it.
Moved out of the drivers because they are not driver code: the conductor's display tests and the colour-bar test card (`user/src/gputest.rs`) and the
protocol constants (`proto`). **The screen size `W`/`H` stays in `gpu.rs`** (it is the display's, a board's driver gives its own 1920x1080).
Open for 3b-ii: the drivers still carry test-only requests (`BLK_CRASH`, `BLK_STALL`, `BLK_EVIL`, `GPU_EVIL`, `GPU_STALL`) that a production driver package
should not contain: they need a build feature.
Checked: QEMU boot identical to before (device, IOMMU and core lines, 222 passes; the four host-peer network tests fail without the peer and pass under
`make quick-net`); the board's user image and kernel were compiled from a scratch copy of the general tree plus the board overlay (nothing in the board
folder touched): both build.
