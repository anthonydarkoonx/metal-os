#!/usr/bin/env python3
"""Encrypted data volume, end to end: boot one disk several times under QEMU with `opt/metallium/only=11` (the conductor drives the lock like a greeter and keeps a
marker file on the volume) and check, from the host, what is on the disk and what can be read back.
Usage: volume-test.py '<qemu command line of the usual tests>'."""
import os, re, shlex, subprocess, sys, shutil, time

cmd = sys.argv[1]
DISK = "build-volume.img"
LFS_BASE = 13312 * 512  # (the data partition's first byte: user/src/storage.rs)
MARKER = b"TOP-SECRET-PAYLOAD-0123456789-metallium"
S1, S2 = bytes(range(32)), bytes(range(32, 64))
for n, s in (("1", S1), ("2", S2)):
    open(f"build-volume-secret{n}.bin", "wb").write(s)


def boot(secret, name):
    c = re.sub(r"file=\S+?,format=raw", f"file={DISK},format=raw", cmd)
    c = re.sub(r"-nographic", "-display none -serial stdio -monitor none", c) if "-nographic" in c else c
    c += " -fw_cfg name=opt/metallium/only,string=11"
    if secret:
        c += f" -fw_cfg name=opt/metallium/device-secret,file=build-volume-secret{secret}.bin"
    log = open(f"build-volume-{name}.log", "w")
    p = subprocess.Popen(shlex.split(c), stdout=log, stderr=subprocess.STDOUT)
    end = time.time() + 300
    text = ""
    while time.time() < end:
        time.sleep(1)
        text = open(f"build-volume-{name}.log", errors="replace").read()
        if "[vol] done" in text or "[vol] volume stays shut" in text or "PANIC" in text:
            break
    time.sleep(1)
    p.kill()
    p.wait()
    return open(f"build-volume-{name}.log", errors="replace").read()


results = []


def check(what, ok):
    results.append(ok)
    print(f"[volume-test] TEST {what}: {'PASS' if ok else 'FAIL'}")


shutil.copy("disk.img", DISK)

a = boot("1", "a")
check("a blank disk on a machine with a secret gets an encrypted volume", "made an encrypted volume" in a and "[vol] state 2 flags 2" in a)
check("files can be written to it", "marker written: OK" in a)
check("a passphrase can be added to the open volume", "set passphrase: 0" in a)
disk = open(DISK, "rb").read()
check("the volume header is on the disk", disk[LFS_BASE:LFS_BASE + 8] == b"MTLMVOL1")
check("the marker file's contents are NOT on the disk in the clear", MARKER not in disk and b"TOP-SECRET" not in disk)
check("the file's name is not on the disk in the clear", b"secret.txt" not in disk[LFS_BASE:])

# control: the same marker on a plain volume (no machine secret, blank disk) IS on the disk, so the check above means something
keep = open(DISK, "rb").read()
shutil.copy("disk.img", DISK)
ctl = boot(None, "ctl")
plain = open(DISK, "rb").read()
check("control: on a machine without a secret the volume is plain and the marker is readable from the disk", "mounted existing store" in ctl or "formatted fresh store" in ctl)
check("control: the marker is in the clear on a plain volume", MARKER in plain)
open(DISK, "wb").write(keep)

b = boot(None, "b")
check("without the secret the volume is locked and says which ways open it (passphrase and machine secret)", "encrypted volume: locked" in b and "[vol] state 1 flags 3" in b)
check("a wrong passphrase is refused (also a long one)", "wrong passphrase: 1" in b and "wrong long passphrase: 1" in b)
check("the right passphrase unlocks it", "[vol] unlock: 0" in b and "state after unlock 2" in b)
check("and the file is there", "marker read back: OK" in b)

c = boot("1", "c")
check("with the machine's secret the volume opens by itself", "opened with the machine's secret" in c and "[vol] state 2" in c and "marker read back: OK" in c)

d = boot("2", "d")
check("another machine's secret does not open it, the passphrase does", "locked, waiting" in d and "[vol] unlock: 0" in d and "marker read back: OK" in d)

# a damaged header: every slot altered
raw = bytearray(open(DISK, "rb").read())
for slot in (0, 1):
    raw[LFS_BASE + 64 + slot * 128 + 52] ^= 0x01
open(DISK, "wb").write(raw)
e = boot("1", "e")
check("a header with altered slots opens with nothing", "locked, waiting" in e and "[vol] unlock: 1" in e and "marker read back: OK" not in e)

for f in ("build-volume-secret1.bin", "build-volume-secret2.bin"):
    os.remove(f)
ok = all(results)
print("== VOLUME: ALL PASS ==" if ok else "== VOLUME: FAILED ==")
if not ok:
    for n in ("a", "ctl", "b", "c", "d", "e"):
        print(f"--- {n} ---")
        print(open(f"build-volume-{n}.log", errors="replace").read()[-1200:])
sys.exit(0 if ok else 1)
