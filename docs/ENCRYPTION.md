# The encrypted data volume

`/data` (the log-structured store) can be encrypted. `/system` is read-only and signed (integrity, not secrecy) and is not.

**Threat model.** Someone who gets the disk (SD card, eMMC, a copied image) but not the running machine. They learn the volume's size and how much of it is
used, nothing about names, sizes or contents of files, and a changed block is noticed (authenticated encryption). Not covered: someone who can run code on the
unlocked machine; rolling a block back to an older copy of itself (no rollback protection: the nonce is random per write, not a counter); an attacker who has
the machine *and* its device secret (see below).

## On the disk
The data partition starts with a **volume header** (4 KiB, `libs/metallium-crypt`): a random 32-byte *volume key* sealed under up to four **key slots**.
A slot is a **passphrase** (key-encrypting key from PBKDF2-HMAC-SHA512, 100 000 rounds, random salt) or the **device secret** (HMAC-SHA512 of the machine's secret
and a salt). Any slot opens the volume; slots change without touching the data (a new passphrase rewrites one slot). Each slot's kind, cost, salt and place are
bound into its seal, so an edited header opens nothing. After the header, every block of the store takes **9 sectors**: 4096 bytes of ChaCha20-Poly1305
ciphertext (random 96-bit nonce per write; the block number and the volume id are authenticated, so a block cannot be moved) and one sector holding the 28 bytes of
nonce and tag. The store above (`metallium-lfs`) sees plain 4 KiB blocks and does not know.

## Policy
* A **blank** data partition on a machine that has a device secret becomes an encrypted volume with one slot, the device secret: **encrypted by default, nobody has to
  type anything**. A machine without a secret (QEMU, and the ROCK 3C until its chip secret is read) makes a plain volume as before.
* A volume made before this existed stays plain and keeps working (it is not converted; a migration would be a separate, explicit step).
* An unlocked volume can add or change a **passphrase** slot (`LFS_SET_PASSPHRASE`). With a passphrase slot, a machine whose secret does not match (the disk moved to
  another machine) can still be opened by typing it.
* A volume that does not open (wrong secret, no passphrase typed) is **locked**: `/data` is not read, the file server does not start serving, and the screen task
  shows the greeter. A volume whose store does not read is **damaged** and is never formatted over.

## Who may do what
* The **device secret** comes from the kernel (`DEVICE_SECRET`, syscall 46) and only to the store server (role 6). QEMU: the fw_cfg file `opt/metallium/device-secret`
  (32 bytes; a test aid, a real machine answers with a chip-unique value). A board's platform file implements `device_secret()`.
* The lock's calls (`LFS_STATUS`, `LFS_UNLOCK`, `LFS_SET_PASSPHRASE`, `proto::volume`) go to the store's endpoint and are accepted from any task that holds a send
  capability to it: the VFS server, the test conductor and the **screen task** (slot 30, so a greeter can unlock). None of them gives access to the files: those need
  a shared page the store assigns by sender.
* The passphrase travels in 16-byte chunks in the message words, is kept only until the last chunk arrives and is zeroed after use.

## Tests
`make host-test` (`metallium-crypt`: HMAC and PBKDF2 against RFC 4231 and Python's `hashlib`, slots, tampering, block sealing), `make volume-test` (one disk booted
six times: created, locked, wrong and right passphrase, secret, wrong secret, damaged header; and the host checks the marker is not on the disk in the clear, with a
control that it *is* on a plain volume) and `make qemu-test-encrypted` (the whole integration suite, crash recovery and cleaning included, on an encrypted volume).

## Not done
The RK3566 chip secret (OTP) for the board's device slot; converting an existing plain volume; rollback protection; a recovery slot UI; changing the number of PBKDF2
rounds from outside; serving `/system` while `/data` is locked (the VFS server waits for the unlock before it serves anything, so programs wait too).
