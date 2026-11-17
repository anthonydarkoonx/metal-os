# Metal OS handoff (rewritten 2026-11-16)

## Where things stand
Metal OS is a Rust `no_std` AArch64 microkernel (QEMU `virt`; a ROCK 3C board port lives in the separate GPL-2.0-only repository `metal-os-rock3c` (formerly `rock 3c test/`; see its `README.md`).
**There is no GUI.** On 2026-11-16 the whole GUI was deleted from the general tree: the libs `metal-classic metal-gui metal-gui-proto metal-taskbar metal-wm
metal-theme metal-font metal-prefs metal-files`, the `shell` (taskbar / Start menu) program, `docs/GUI-SPEC.md`, the icon code and the GUI test images. Earlier
GUI generations (retro blue, flat pastel, SerenityOS-style classic) and the programs that went with them (browser, editor, Control Panel, Files, Task Manager,
Paint, dialogs, desktop icons) are gone too; none of it is in the tree or in git history (the repository was first committed on 2026-11-16, after the removal).
**What the screen shows now:** `user/src/screen.rs` is a stub: a backdrop with the terminal (`user/src/shell.rs`, `console.rs`, drawn with the bitmap font in
`libs/metal-gfx`) centred on it, keyboard input only. `make run-window` boots to it (checked by screenshot on QEMU, 2026-11-16; the test suites were not run after the removal).
**Kept:** kernel, drivers (`gpu.rs`, `kbd.rs`, `blk.rs`, `netdrv.rs`, board overlay), `metal-gfx` (only for the terminal's font and a few drawing calls), `metal-input`,
`metal-edit` (terminal editor), `metal-map`, `metal-ed25519`, the storage, net and compression libs, the program manager and the terminal.
**Naming (2026-11-17):** the screen task, once "the desktop", is now the **console** (role 13, task name `console`, `kernel/src/devices.rs` `Phase::Console`, `CONSOLE_HEAP_PAGES`, `user/src/screen.rs`, `INFO_CONSOLE`, `programs::console_session()`). The role number 13 and the board's `DESK_*` display constants keep their numbers.
**Desktop seam (2026-11-17):** `make run-window DESKTOP=<path to a desktop repo>` links that repo's `metal-desktop-boot` into the screen task (`user/src/screen.rs`, `#[cfg(desktop)]`) and puts its packages in the image, installed by default (`/system/etc/program-defaults`, `role = desktop`); see `docs/DESKTOP-INTERFACE.md`. Default builds are unchanged. With `../metal-os-desktop` the compositor, taskbar and Start menu boot on QEMU; the other desktop programs are not packaged yet.
**Leftover wiring (harmless, can go with a future GUI decision):** the display endpoint that `progmgr` still hands to programs with the `window` permission (`docs/PROGRAMS.md`, `metal-map`'s permission list).
**Memory safety (2026-11-17):** see `docs/BOARD-SYNC.md` and the git log. Miri passes on all the libs' tests; the long ones run reduced under Miri (`cfg!(miri)`: strided byte sweeps, 2 RFC 8032 vectors, 2,000 loader images; native counts unchanged). Deny-level lints `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks` apply to every crate (`make clippy`). Every `unsafe` site has a SAFETY comment; the kernel's lock-free globals are borrow-checked at run time (`kernel/src/sync.rs`); the board runs the new kernel (self-test 88/13/1, same as before).
**Repository:** https://github.com/anthonydarkoonx/metal-os (public; disk images, signing keys and build output are not in it; the board port is https://github.com/anthonydarkoonx/metal-os-rock3c).
**Rules from the user:** OS changes only in the general tree, the board folder holds drivers only (`docs/BOARD-SYNC.md` logs every change); say "program", not "app";
do not add programs nobody asked for; run only fast host tests unless asked.

## SBC port-readiness audit (2026-11-13)
Read-only audit written up in `docs/PORTING.md`: what is portable (core kernel logic, all libs, programs), what is QEMU-`virt`-specific (RAM map and 256 MiB frame tables, load address, no DTB use, PL011 at a fixed address, GICv2 only, PSCI over HVC, PL031 RTC, ECAM/virtio-only PCI, SMMUv3-based driver confinement, virtio-rng, the virtio drivers) and a suggested order (a per-target platform layer inside the framekernel's framework side, serial console first, firmware framebuffer, storage, USB input, network). Open design question: boards without an IOMMU. Not verified against any real board's documentation.


## Kernel layering (framekernel style, 2026-11-13)
The kernel has a *frame* (`kernel/src/frame.rs`: `phys` = bounds-checked physical RAM access, `cpu` = system registers / wfe / semihosting) plus the other framework modules that still use `unsafe` (`mmu`, `smmu`, `gic`, `trap`, `smp`, `sync`, `timer`, `cache`, `kaslr`, `pci`, `platform`, `uart`, `virtio_rng`, `main`, `boot.S`). These 15 modules carry `#![forbid(unsafe_code)]`: `cap ipc compact irq mem ring swap obj spawn syscall pager thread task sched rand`. Rules: new raw-memory or register access goes into `frame`; addresses a user program chose use the `try_*` forms (a device page is not RAM; never panic on user input). `sync::Global::get()` returns a run-time-checked guard (`RefMut`), so an overlapping borrow is a kernel panic, not two live `&mut`; the unchecked `boot_get` is `unsafe` (only `mmu::init`). Checked with a build and one headless `ui-probe` boot, not the QEMU suites. Next (not started): measure IPC (benchmark endpoints 6..15 exist), then direct hand-off / fewer round trips.


## Programs (phases 1 and 2), see docs/PROGRAMS.md
- `.map` packages (`libs/metal-map`, `tools/mkmap.py`), the program manager `progmgr` (an ordinary `programs` program, no kernel part), the ability lifecycle (`programs/src/ability.rs`), supervision and restart, rights from the manifest, Ed25519-signed packages with a trust store and policy in `/system/etc` (`libs/metal-ed25519`), and per-program private storage (VFS `SCOPE`, kernel reports a program's parent in SYSINFO page 5, `metal_loader::spawn_with`).
- `make run-window` skips the tests, starts the manager and gives the screen task to the terminal (`user/src/screen.rs`, terminal only). Terminal commands: `prog list|available|install|start|stop|fg|bg|state|info|uninstall`.
- Tests: the scenario in `programs/src/bin/progctl.rs` (run last by `user/src/programs.rs`, deliberately *after* the leak checks: reading packages raises the services' heap high-water marks and ate into the 300-programs check's 64-frame slack), `cargo test -p metal-map -p metal-ed25519`, `make map-check`. Full `make test` passed on 2026-11-12 after the last change.
- `make test` runs its suites on ONE disk image, so `/data` carries over between them: the scenario cleans up before and after itself.
- Test signing keys are public (`tools/keys/`). Not done: manager restart, service registry, measured boot of the image.
- Looking at the QEMU window without a mouse: QMP socket `build-window.sock` (`screendump`, `send-key`).


## Latest full run: `make test`, all pass (`full-test.log`)
- Host unit tests: all `ok`.
- `qemu-test` on 4 CPUs and on 1 CPU (`qemu-test-up`): `ALL PASS` both times, 425 TEST lines PASS and 0 FAIL.
- `qemu-test-noncoherent`, `aslr-replay`, `entropy`, `kaslr`, `service-maps`, `stack-guard`, `fuzz` (seed 1): all `ALL PASS`.
- The `[program] panic: memory allocation of 65536 bytes failed` lines are expected. They come from the test "a program that allocates without checking is stopped at its limit", which passes.

## Done and verified
- **Item 1: boot services use kernel-chosen addresses.** Boot services map with address 0, and the kernel refuses fixed addresses from them (`spawn::restrict_boot_task`, `user/src/main.rs` `auto_maps`/`va`).
  - The missing test now exists: `make service-maps` boots 3 times with `opt/metal/map-log=1`, and `tools/check_service_maps.py` compares the `[map]` lines. Last result: 52 service mappings, 1 with the same address in all 3 boots. That is within the allowed chance collisions (at most 1 in 20).
  - Test programs (conductor, test server, fuzzer, bench, monkey, spinner, `vfs::program`) still use explicit addresses on purpose. Not rechecked since 10-28.
- **Item 2: scheduler up-migration test.** `user/src/sched.rs` section 1b: a CPU hog pushed onto an efficiency core goes back to a performance core once one is idle. It passes with "stuck in 0 of 8 rounds". The kernel side is in `kernel/src/sched.rs`, in the "Heavy work that spilled onto an efficiency core…" block.
- **Item 3: `qemu-test-up` flake.** It has not come back. Single-CPU runs passed on 10-28 (4 in a row), on 11-03 (`run-qemu-test-up.log`) and on 11-04 (in `full-test.log`). The threads-kill test reports "5 slots in use: 5 … slots left 0". The cause was never proven. The live-slot debug print in `user/src/programs.rs` (~line 929) is still there and would name any leftover program.
- **Item 4: entropy hardening.** `kernel/src/virtio_rng.rs` reads 32 bytes once at bus probe, behind its own SMMU stream, then shuts the device down. `noaslr`, `aslr-seed=` and `aslr-log` each print a `[rand] WARNING` and skip the device. A boot with no RNDR and no seed file warns twice. `make entropy` checks all of this. Documented in `docs/ABI.md`, address-space layout section.
- **Item 5: kernel stack probing.** Test hooks `opt/metal/stack-overflow=1`, `stack-overflow-core1=1` and `stack-overflow-bigframe=1` (`kernel/src/main.rs`, `overflow_stack_bigframe`). The bigframe hook uses a 128 KiB frame, larger than stack plus guard. `make stack-guard` checks cores 0 and 1 and the big frame. Documented in `docs/ABI.md`, "Kernel stacks".
- **Item 6: KASLR.** The kernel is a PIE linked at 0x4008_0000. At boot (`kernel/src/kaslr.rs`, `boot.S`) it relocates itself, copies itself to a random page-aligned place in RAM (clear of the first 512 KiB and of the old image), and continues there. The old image is freed. This gives about 14 bits of randomness with 64 MiB of RAM. `make kaslr` boots 8 times and got 8 different addresses, all inside RAM. It also boots once with 16 MiB. Documented in `docs/ABI.md`, "Kernel address".
  - Known limits (documented): the boot log prints kernel addresses. `noaslr` and the seed modes do not fix the kernel address. With unknown RAM size the image stays put, with a warning.
- **PIE / ET_DYN support** (from before): `TASK_BASE` syscall (39), `FLAG_PIE`, loader rebase, random image bases, and the boot user image is a PIE ELF. See `docs/ABI.md`.

## Not re-run since the 11-04 changes
- **`make chaos`**: last `ALL PASS` (seed 1) on 11-03 14:47 (`run-chaos.log`). That was before the 11-04 edits to `kaslr.rs`, `sched.rs`, `syscall.rs`, `platform.rs`, `rand.rs` and `user/src/main.rs`/`sched.rs`. It is not part of `make test`, so re-run it.
- **`make inherit-ab`** and **`make provenance-check`**: no recent log.
- Only one full `make test` has run since the last edits. The earlier flake history suggests repeating `qemu-test-up` a few times before calling it stable.

## Open questions / loose ends
- `run-qemu-test.log` (11-03 14:40) says `QEMU INTEGRATION: FAILED (qemu rc=0)` with no failing TEST line. The log ends after the scheduler stats. Later runs pass, but nobody investigated that failure. Most likely the run was cut short or the summary check missed a line.
- The top-level directory has about 30 old `*.log`/`*.sum` files. `full.sum`, `up.sum` and `suites.log` are from 10-28 and out of date. They can be deleted or ignored. `full-test.log` is the current reference.

## Gotchas
- **No atomic read-modify-write before the MMU is on.** With the MMU off every access is Device memory; on the RK3566 an atomic there is a data abort (ESR DFSC 0x35), but QEMU allows it, so the QEMU suite cannot catch this. `mmu::init` therefore uses `Global::boot_get` (plain access), not the checked `get`. Run the board self-test after any change to early boot.
- The folder used to be called `Metal OS`. If `make` fails with `cannot find linker script /home/computer/Desktop/Metal OS/user/link.ld`, cargo has cached the old path. `touch user/link.ld programs/link.ld programs/link-pie.ld kernel/linker.ld` and rebuild. Checked 2026-11-12: after that, `make run` boots (KASLR moves the kernel, virtio-rng is folded in, the boot tasks start).
- Never run two `make` jobs at once (they share `disk.img` and ports). A killed make can leave QEMU or `tools/netpeer.py` holding ports 18077/7002, so check for leftovers first.
- When waiting, loop on `pgrep -x make`. `pgrep -f "make qemu-test"` matches its own shell wrapper and never ends.
- No GPL-derived code, clean-room only. Ask before reading Linux sources.
- Scope is QEMU-only, general-purpose kernel work.
