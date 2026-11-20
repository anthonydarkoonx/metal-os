# Desktop interface

Metallium-OS has no desktop in its tree. A desktop environment is a separate repository that the OS image can be built with. Without
one the screen task (role 13, `user/src/screen.rs`) is the built-in terminal console.

## Using one

```sh
make run-window DESKTOP=../metallium-os-desktop
```

`DESKTOP` is the path of a repository that contains a crate `metallium-desktop-boot` (below). The Makefile builds that crate for
`aarch64-unknown-none-softfloat` (pointing its `metallium-driver-rt` and `metallium-gfx` git dependencies at this tree's copies), then builds the
user image with `--cfg desktop --extern metallium_desktop_boot=...`. Nothing in `Cargo.toml`, `Cargo.lock` or the kernel names the desktop:
a build without `DESKTOP` is the same as before.

## The contract

The desktop's crate `metallium-desktop-boot` exports one function:

```rust
pub fn metallium_desktop_run(display: u64, gpu: u64, input: u64, framebuffer: u64, fb_handle: usize, manager: u32, power: u64) -> !
```

The screen task calls it instead of starting the console, and never gets control back. The arguments are plain integers, so the OS needs
none of the desktop's types:

| Argument | Meaning |
|---|---|
| `display` | slot of the display endpoint to receive on (the one `progmgr` hands to programs with the `window` permission) |
| `gpu` | slot of the display driver's endpoint (`GPU_INFO`, `GPU_FLUSH2`) |
| `input` | slot of the input driver's endpoint (`INPUT_POLL`, and `INPUT_WAIT`: the same, but the call waits up to `w[1]` ms, at most 1000, for an event instead of answering at once with none; the driver does nothing else meanwhile, so one client should use it) |
| `framebuffer`, `fb_handle` | slot of the scanout buffer's memory capability, and the handle to map it at (`metallium_driver_rt::mem_map`, `va`) |
| `manager` | the program manager's task id, 0 if there is none |
| `power` | slot of the power right (`R_POWER`), 0 if there is none |

If there is no display endpoint (no program manager) the console runs instead.

Notes for a desktop's author:
- The screen task is a boot task: the kernel refuses fixed mapping addresses. A crate that has its own copy of `metallium-driver-rt` (the usual case)
  must call `metallium_driver_rt::auto_maps()` first, because that copy has its own statics.
- The desktop crate is built with `-C relocation-model=pic -Zembed-metadata=yes` and fat LTO so the user image's own LTO can use it.

## The desktop's programs

A desktop brings its programs as packages, installed by default:

- The desktop repository has an `os/` directory: a Cargo workspace of `no_std` programs (`os/src/bin/<program>.rs`, using the `metallium-programs` crate),
  `os/packages/<id>.manifest` for each package, and `os/defaults`.
- `make disk DESKTOP=<path>` builds them as position-independent programs (`METALLIUM_OS_DIR` names this checkout, for the linker script), and
  `tools/mkmap.py` signs the packages with the dev key and puts them in `/system/usr/programs` (`METALLIUM_EXTRA_PACKAGES`).
- `os/defaults` becomes `/system/etc/program-defaults`: one program id a line, `id autostart` to start it too. The program manager installs each
  on first start (a note in `/data/programs`, not a copy) and starts the `autostart` ones. A default program the user uninstalls stays uninstalled
  (a `.gone` note).
- A desktop's command-line shell (a terminal that runs programs) declares `permission = spawn` (`docs/PROGRAMS.md`, *Starting programs*). The screen task's own
  capabilities beyond the arguments above (its spawn capability, VFS session, endpoint factory) are the console's and are **not** part of this contract.
- The manifest key `role = desktop` marks the taskbar program: it implies `permission = shell`, and the manager tells the compositor
  (`MGR_SET_ROLE`) once the program has started. The compositor takes requests for that role only from the manager.

## Status (2026-11-17)

Boots in QEMU (`make run-window DESKTOP=../metallium-os-desktop`): the compositor and the `desktop` program (taskbar, Start menu, clock, installed-programs list)
run on the real system calls, installed and started by default. Not done: the desktop's other programs (terminal, files, settings, task manager,
capabilities manager: each needs a program and an OS-side `Env`), and the OS-side features the desktop's design lists (`docs/DESKTOP.md` in the desktop
repository, §14: `KILL_OF`, start with an argument, `opens`). The test suites were not run with a desktop linked in.
