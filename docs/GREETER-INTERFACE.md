# The greeter seam

While the encrypted data volume is **locked** (`docs/ENCRYPTION.md`) the screen task runs a greeter before anything else, so the user can type the passphrase.
The OS has a built-in text prompt (`user/src/greeter.rs`); a **greeter of its own**, in a separate repository, replaces it the way a desktop does
(`docs/DESKTOP-INTERFACE.md`).

```
make run-window GREETER=../greeter             (or any make target that builds the user image)
```
builds that repository's crate `metallium-greeter-boot` (`make greeter-lib`: `cargo build --release --target aarch64-unknown-none-softfloat -p metallium-greeter-boot
--target-dir target-metallium-os`, with the OS's `metallium-driver-rt` and `metallium-gfx` patched in, as for a desktop) and links it into the screen task. It can be combined with
`DESKTOP=`.

```rust
pub fn metallium_greeter_run(gpu: u64, input: u64, store: u64, sessions: u64, framebuffer: u64, fb_handle: usize)
```

| Argument | Meaning |
|---|---|
| `gpu` | slot of the display driver's endpoint (`GPU_INFO`, `GPU_FLUSH2`; `metallium_driver_rt::proto::gpu`) |
| `input` | slot of the input driver's endpoint (`KBD_POLL` for characters, `INPUT_POLL` / `INPUT_WAIT` for events; `proto::input`) |
| `store` | slot of the data store's endpoint: the right to ask about and unlock the volume (`proto::volume`) |
| `sessions` | slot of the session service's endpoint (`sessiond`: accounts, login, recovery; its protocol is in `programs/src/bin/sessiond.rs`), or 0 if the system has none yet: then the greeter only unlocks the volume |
| `framebuffer`, `fb_handle` | slot of the scanout buffer's memory capability and the handle it is mapped at (the screen task has mapped it; `metallium_driver_rt::mem_map` if the greeter wants its own mapping) |

It **returns** once the volume is open (`LFS_STATUS` reports `state::UNLOCKED` or `PLAIN`); the screen task then carries on (the console, or the desktop). If it returns
with the volume still locked the built-in prompt takes over. A crate that has its own copy of `metallium-driver-rt` must call `metallium_driver_rt::auto_maps()` first, as a desktop does.

**Protocol** (`libs/metallium-driver-rt/src/proto.rs`, `volume`):
* `call(store, [LFS_STATUS, 0, 0])` -> `[state, flags, 0]`; `state`: 0 plain, 1 locked, 2 unlocked, 3 damaged; `flags`: bit 0 a passphrase opens it, bit 1 the machine's own secret opens it.
  A greeter can show "enter your passphrase" only when bit 0 is set.
* `LFS_UNLOCK`: the passphrase (at most 128 bytes) in chunks of 16: `w[0] = LFS_UNLOCK | total_len << 8 | offset << 24`, `w[1]`, `w[2]` the 16 bytes at `offset` (zero padded).
  The reply is `MORE` (4) until the last chunk, then 0 (unlocked), 1 (wrong), 2 (not locked) or 5 (malformed). A wrong passphrase costs the PBKDF2 time (about a second on
  the board); nothing limits the tries, so a greeter that wants a delay or a counter adds it.
* `LFS_SET_PASSPHRASE`: the same chunks, on an unlocked encrypted volume: sets or changes the passphrase (a settings screen, or a first-login "choose a passphrase").

A greeter is an ordinary part of the screen task: it must not write the passphrase anywhere, and should zero its copy after the last chunk. The file system does not
start serving until the volume is open, so a greeter has no `/data` (or `/system`) to read: it draws what it has built in.

## A greeter that exists
[`metallium-greeter`](https://github.com/anthonydarkoonx/metallium-greeter) implements this contract: unlock, first-boot setup (the administrator and a recovery key shown once), login, recovery
with the key. `make run-window GREETER=../metallium-greeter` links it in; `make greeter-test GREETER=../metallium-greeter` builds it with its `log-key` feature (the test reads the key from the
console to type it back; never in a real image) and drives the screens through QEMU (`tools/greeter-test.py`: first boot, wrong and right passphrase, a second boot with the recovery).
The screen task runs the greeter when the volume is locked or a session service answers (`sysinfo(2)[3] == 14` for now: the boot flow does not start `sessiond` yet, and the screen task's and the
conductor's capability tables are full enough that the session-service endpoint, `EP_SESSIONS` (14), is installed only in the test modes 13 and 14).
