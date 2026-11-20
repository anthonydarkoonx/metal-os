# Multiple users: a plan (2026-11-20, not built)

## What the OS already gives
* **Processes are isolated by capabilities**, not by user ids: a task can touch another task, a file or a device only through a capability it was given. So *process*
  separation between users is free: two sessions that share no capabilities cannot see each other.
* **Files are the weak point**: the VFS is one namespace; separation today is per *program* (`V_SCOPE`: a program is confined to `/data/programdata/<id>`, set by its parent
  before it runs; `allfiles` and `admin` are permissions in the manifest and the stored policy).
* The **program manager** (`progmgr`) holds spawn/revoke/power authority and keeps per-program policy in `/data/programs`.
* The **screen task** owns the display and keyboard, and (since the encrypted volume) runs a greeter before anything else.
* **Encryption**: one volume key, four key slots (`docs/ENCRYPTION.md`).

## Model
A **user** is a name, a numeric id (16 bits, an index, never reused while the account exists), a role (**admin** or **standard**), a home directory and a **credential**
(a passphrase). There is no uid in the kernel. A **session** is the set of tasks one login started; it is the unit of isolation and the unit of cleanup.
* **Isolation of processes**: free (capabilities). A session's tasks get only capabilities derived from that session's own manager.
* **Isolation of files**: the VFS confines a session to its home with the mechanism programs already use (`V_SCOPE`), extended from "a directory" to "a user": `/home/<name>`
  is `/` for the user's desktop and programs; `/data/shared` is the only common place, and `/system` stays read-only for everyone.
* **Authority**: only an admin session's manager holds the privileged capabilities (power, revoke authority, install-for-everyone, creating users). A standard user's manager
  gets a narrowed copy (no `admin`, no `allfiles`).

## Parts
1. **Account store** (`/data/accounts`, owned by the session service, readable by nobody else): per user id, name, role, home key slot (below), created/last login. No password
   hashes: a credential is a key slot, so there is nothing to crack offline other than the slot itself.
2. **Session service** (a new small boot task, between the greeter and the program manager): `LOGIN(name, passphrase) -> session`, `LOGOUT`, `CREATE_USER`, `SET_CREDENTIAL`,
   `LIST`. On login it starts **one program manager per session** (own control endpoint, own `/home/<name>/.programs` policy, own spawn budget) and the user's desktop; on
   logout it kills the session's tasks (the manager's revoke authority covers them) and revokes its capabilities.
3. **VFS**: a session id with every client (the kernel already tells the VFS which task is calling; the session service tells it which tasks form a session); homes are
   scoped at the root, not per program; per-user quotas (entries and bytes) replace the single global small limit.
4. **Greeter**: asks for a user name and a passphrase (and, first boot, creates the first admin); the contract grows from `LFS_UNLOCK` to `LOGIN`.
5. **Encryption per user** (the part that makes it real): see below.

## Encryption per user (recommended)
One machine-wide volume key (device secret) protects the system data and the accounts; **each home is its own encryption domain** with its own random key, sealed in a
slot opened by that user's passphrase. The store tags every block with its domain id (authenticated, like the volume id today); the VFS says which domain a file belongs to; the
store keeps only the keys of unlocked domains. A locked home is unreadable *by other users, by an admin, and by a copied disk* until its owner logs in. Costs: the store needs a
domain id in `LFS_PUT/GET` (the on-disk block format already carries room in the authenticated data, so a format bump is small); the directory structure of a home must live in
that home's blocks; an admin who forgets a user's passphrase cannot recover the data (an optional escrow slot, off by default).

## Decisions (2026-11-20)
1. **Several sessions at once, switched with Ctrl+Alt+F1..F12.** F1 is the default session's tty (the first login at boot lands there). F2..F12 each show a login prompt until
   someone logs in on them; a logged-in tty keeps its session alive while another is in front. One live session per user: logging in as a user who already has a session
   says so and offers to switch to its tty.
2. **Encryption per user** (each home its own key, opened by that user's passphrase).
3. **One admin, created at first boot** (the first-boot greeter is a setup screen: name, passphrase, recovery key). More users are created by the admin afterwards.
4. **Program configuration is per user** (GlieseWM's config and every program's data live under the user's home; `GLIESE_CONF` becomes `~/.config/gliese/gliese.conf`).

## TTYs: what has to change
Today the desktop is linked into the one screen task, so there can only be one. Several concurrent sessions need a **tty manager** (the screen task keeps this one job)
and a **desktop per session as its own task**:
* The tty manager owns the real framebuffer and the input driver. Each tty has a **virtual framebuffer** (a memory object) and speaks the *same protocols* the desktop
  already uses: it serves `GPU_INFO` / `GPU_FLUSH2` for the tty's virtual display and `INPUT_POLL` / `INPUT_WAIT` for its virtual keyboard and pointer. Only the **active** tty's
  flushes reach the scanout (inactive ones just update their buffer), and only the active tty receives input. A desktop therefore does not know it is not alone:
  GlieseWM's `gpu` and `input` slots simply point at the tty manager's endpoints instead of the drivers'.
* The tty manager reads input first and **intercepts Ctrl+Alt+F1..F12** (like the attention key is today), switches the active tty and flushes its buffer.
* The **session service** starts, per login, a desktop task (the desktop seam grows a second form: a program entry point with the display/input/framebuffer capabilities,
  instead of a function linked into the screen task) and the session's program manager. A tty with no session runs the **greeter**, one instance per tty.
* Costs: memory (a back buffer per tty: 1920x1080 is 8 MiB; allocate lazily, only for ttys in use), and the display's vsync-paced copies are shared.
* Risk: the desktop contract changes (`metallium_desktop_run` as a task entry point), so GlieseWM and any other desktop need a matching change.

## Recovery
A forgotten passphrase must not mean a brick, and must not mean the admin can read everyone's files. Options, and what I would do:
* **No recovery**: strongest, and a forgotten passphrase loses that home for good. Right for a user who opts out.
* **Admin escrow**: every home key also sealed for the admin. Easy support, but the admin can read all homes and the admin's passphrase becomes the one secret that matters.
  Rejected as a default.
* **Recovery key (recommended default)**: at account creation a random recovery key (say 24 words, shown once on screen, to be written down) is sealed into one more
  slot of that home. It opens that home only. Nobody else holds it, nothing is escrowed, and the user (or the admin acting for them, with the key in hand) can reset the
  passphrase. A user may decline it at creation (then: no recovery).
* **Reset without the data**: the admin can always **wipe and recreate** a home (new key, empty files), so a locked-out user gets an account back, never the old data.
* **The machine volume**: the device secret binds the system data to one chip (a replaced board, a copied card). At first boot the admin also gets a recovery key for the machine
  volume. Without it a dead board means the system data is gone (the homes are separate, and open with their own passphrases/recovery keys on a rebuilt system).
First boot therefore shows two things to write down: the admin's home recovery key and the machine recovery key.

## Phases and tests (revised order)
| Phase | What | Test |
|---|---|---|
| 1 | Accounts, session service, one desktop and program manager per session, VFS homes (session-scoped), first-boot setup greeter, one tty | two users (one at a time): A cannot list, open or create in B's home (no `..`, no absolute paths); A's programs cannot reach B's tasks; logout leaves no task of the session |
| 2 | Per-user encryption domains and recovery keys | per user, the marker test of `make volume-test`: B's marker unreadable without B's key even with the machine unlocked; recovery key opens only its home; wrong key opens nothing |
| 3 | Tty manager, virtual displays/inputs, Ctrl+Alt+F1..F12, a desktop as a task, concurrent sessions | two live sessions: the display and input follow the active tty; the other keeps running; switching back restores its screen; F-keys never reach the desktop |
| 4 | Admin model: create/remove users, system-wide installs, power, wipe-and-recreate | a standard user cannot do any of those; revoke authority is absent from a standard session |
| 5 | Board: the chip secret (OTP) for the machine volume, then the same tests on the ROCK 3C | volume and session tests on hardware |

## Known dependencies and risks
* The data store is **small and its directory is one in-memory table** (`Datafs`, scoped programs get 12 entries): homes need a real directory structure, per-user quotas and a
  bigger store before more than a couple of users is practical.
* The VFS must learn session identity without trusting clients: the kernel's sender id plus the session service's table (the same trust the VFS already puts in a program's
  parent for `V_SCOPE`).
* Nothing here changes the kernel's isolation guarantees; the risk is in the services (session service, VFS, greeter), which are all in the trusted base of a login.
