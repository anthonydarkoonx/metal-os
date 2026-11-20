#!/usr/bin/env python3
"""Sessions end to end (`opt/metallium/only=13`): the conductor is the greeter against `sessiond`; each logged-in user's session is a program confined to /data/home/<user>.
Checks the conductor's lines, what each session reported about itself, and (from the host) that no passphrase is on the disk.
Usage: session-test.py '<qemu command line of the usual tests>'."""
import re, shlex, subprocess, sys, time

encrypted = len(sys.argv) > 2 and sys.argv[2] == "encrypted"  # (the machine volume is encrypted too: no marker is readable from the disk at all)
cmd = sys.argv[1] + " -fw_cfg name=opt/metallium/only,string=13"
log = open("build-session.log", "w")
p = subprocess.Popen(shlex.split(cmd), stdout=log, stderr=subprocess.STDOUT)
end = time.time() + 400
text = ""
while time.time() < end:
    time.sleep(1)
    text = open("build-session.log", errors="replace").read()
    if "[session] done" in text or "PANIC" in text:
        break
time.sleep(1)
p.kill()
p.wait()
text = open("build-session.log", errors="replace").read()
results = []


def check(what, ok):
    results.append(ok)
    print(f"[session-test] TEST {what}: {'PASS' if ok else 'FAIL'}")


for l in text.splitlines():
    if l.startswith("[session] TEST"):
        ok = l.endswith("PASS")
        results.append(ok)
        print(l)
for user in ("alice", "bob"):
    tag = re.escape(f"[homeprobe {user}]")
    check(f"{user}'s session writes and reads its own file", f"[homeprobe {user}] own file written and read back: yes" in text)
    check(f"{user}'s session cannot see the account store or the other homes", f"[homeprobe {user}] sees the account store: no" in text)
    check(f"{user}'s session cannot name .. or /home", f"[homeprobe {user}] can name .. : no" in text and f"[homeprobe {user}] can name /home or /accounts directly: no" in text)
    check(f"{user}'s /data holds only {user}'s own file", f"[homeprobe {user}] listing of /data: note.txt\n" in text)
    m = re.search(rf"\[homeprobe {user}\] files made until the quota stopped it: (\d+)", text)
    check(f"{user}'s home has a quota (made {m.group(1) if m else '?'})", bool(m) and 55 <= int(m.group(1)) <= 64)
    check(f"{user}'s child program is confined to the same home (cannot see the account store, sees the parent's file, writes into it)",
          f"[homeprobe {user}/child] sees the account store: no" in text and f"[homeprobe {user}/child] sees the parent's note: yes" in text and f"[homeprobe {user}] child ended (yes), its file is in my home: yes" in text)
check("the administrator is made by the first CREATE", "account admin created (uid 1, the administrator)" in text)
disk = open("disk.img", "rb").read()
check("no passphrase is on the disk in the clear", all(w not in disk for w in (b"admin-passphrase-1", b"alices-passphrase", b"alices-new-passphrase", b"bobs-passphrase", b"bobs-changed-passphrase")))
# the volume here is plain (no machine secret), so what is encrypted on this disk is encrypted by the *users' domains*: the system's marker is readable, the users' files are not
if encrypted:
    check("the machine volume is encrypted: not even the system's marker is readable from the disk", b"SYSTEM-MARKER" not in disk and b"MTLMVOL1" in disk[13312 * 512:13312 * 512 + 8])
else:
    check("control: the system's own file is in the clear on the plain volume", b"SYSTEM-MARKER-in-the-clear-0123456789" in disk)
check("the users' files (alice's, bob's, and their children's) are NOT in the clear: they are sealed under their domains' keys",
      all(w not in disk for w in (b"note of alice", b"note of bob", b"child of alice", b"child of bob")))
ok = results and all(results) and "[session] done" in text
print("== SESSIONS: ALL PASS ==" if ok else "== SESSIONS: FAILED ==")
if not ok:
    print(text[-3000:])
sys.exit(0 if ok else 1)
