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

## Sessions: one at a time or several
* **Phase 1: one session at a time.** Login starts it, logout ends it and returns to the greeter. Simplest, and enough for a board with one screen and keyboard.
* **Phase 2: fast user switching.** Several sessions stay alive; the screen task gives the display and input to the active one (the others keep running, or are suspended if
  the kernel can freeze a session: a scheduler class change per session). Needs the screen task to multiplex and the input driver to route events by the active session.

## Phases and tests
| Phase | What | Test |
|---|---|---|
| 0 | Decide the open questions below | |
| 1 | Accounts, session service, per-session progmgr, VFS homes, greeter login, one session at a time | two users on QEMU: A cannot list, open or create in B's home (and the VFS refuses `..` and absolute paths); A's programs cannot reach B's tasks; logout leaves no task of the session |
| 2 | Per-user encryption domains | the marker test of `make volume-test`, per user: B's marker is not readable without B's passphrase even with the machine unlocked; a wrong passphrase for B does not open A's |
| 3 | Admin model: roles, who may install system-wide, create users, power | a standard user cannot install for others or create users; revoke authority is absent from a standard session |
| 4 | Fast user switching | two live sessions, the display follows the active one, input only reaches it |
| 5 | Board: the chip secret for the machine volume (OTP), then the same tests on the ROCK 3C | `fetch` and the volume tests on hardware |

## Open questions (decide before phase 1)
1. **One session at a time, or fast user switching** (phase 4 now or later)?
2. **Per-user encryption domains** (recommended) **or one volume key** with the VFS alone enforcing separation (simpler; a user's data is protected from another user only by
   the OS's own correctness, not by cryptography)?
3. **Admin model**: one admin created at first boot, or any user can be promoted; is there a **recovery** path (escrow slot, or no recovery by design)?
4. **Do the desktop (GlieseWM) and its config live per user** (yes in this plan: `~/.config`-style files under the home, `GLIESE_CONF` becomes per user)?

## Known dependencies and risks
* The data store is **small and its directory is one in-memory table** (`Datafs`, scoped programs get 12 entries): homes need a real directory structure, per-user quotas and a
  bigger store before more than a couple of users is practical.
* The VFS must learn session identity without trusting clients: the kernel's sender id plus the session service's table (the same trust the VFS already puts in a program's
  parent for `V_SCOPE`).
* Nothing here changes the kernel's isolation guarantees; the risk is in the services (session service, VFS, greeter), which are all in the trusted base of a login.
