# Desktop interface

Metal OS has no desktop in its tree. A desktop environment is a separate repository that the OS image can be built with. Without
one the screen task (role 13, `user/src/screen.rs`) is the built-in terminal console.

## Using one

```sh
make run-window DESKTOP=../metal-os-desktop
```

`DESKTOP` is the path of a repository that contains a crate `metal-desktop-boot` (below). The Makefile builds that crate for
`aarch64-unknown-none-softfloat` (pointing its `metal-driver-rt` and `metal-gfx` git dependencies at this tree's copies), then builds the
user image with `--cfg desktop --extern metal_desktop_boot=...`. Nothing in `Cargo.toml`, `Cargo.lock` or the kernel names the desktop:
a build without `DESKTOP` is the same as before.

## The contract

The desktop's crate `metal-desktop-boot` exports one function:

```rust
pub fn metal_desktop_run(display: u64, gpu: u64, input: u64, framebuffer: u64, fb_handle: usize, manager: u32, power: u64) -> !
```

The screen task calls it instead of starting the console, and never gets control back. The arguments are plain integers, so the OS needs
none of the desktop's types:

| Argument | Meaning |
|---|---|
| `display` | slot of the display endpoint to receive on (the one `progmgr` hands to programs with the `window` permission) |
| `gpu` | slot of the display driver's endpoint (`GPU_INFO`, `GPU_FLUSH2`) |
| `input` | slot of the input driver's endpoint (`INPUT_POLL`) |
| `framebuffer`, `fb_handle` | slot of the scanout buffer's memory capability, and the handle to map it at (`metal_driver_rt::mem_map`, `va`) |
| `manager` | the program manager's task id, 0 if there is none |
| `power` | slot of the power right (`R_POWER`), 0 if there is none |

If there is no display endpoint (no program manager) the console runs instead.

Notes for a desktop's author:
- The screen task is a boot task: the kernel refuses fixed mapping addresses. A crate that has its own copy of `metal-driver-rt` (the usual case)
  must call `metal_driver_rt::auto_maps()` first, because that copy has its own statics.
- The desktop crate is built with `-C relocation-model=pic -Zembed-metadata=yes` and fat LTO so the user image's own LTO can use it.
- The desktop's programs (taskbar, files, ...) are ordinary `.map` packages for `progmgr`; they are not part of this interface.
  Today the image gets no packages from a desktop: with `DESKTOP` set you get the compositor, the desktop surface and the pointer, and
  the programs still have to be built, signed and installed.

## Status (2026-11-17)

Boots in QEMU (`make run-window DESKTOP=../metal-os-desktop`): the compositor runs on the real system calls and draws the desktop surface and
the pointer. Not done: building and installing the desktop's programs, and the OS-side features the desktop's design lists (`docs/DESKTOP.md` in the
desktop repository, §14: `KILL_OF`, start with an argument, `opens`). The test suites were not run with a desktop linked in.
