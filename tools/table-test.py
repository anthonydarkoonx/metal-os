#!/usr/bin/env python3
"""The /data directory table (`opt/metallium/only=12`): an older-layout table is read and rewritten, hundreds of entries survive a restart, long paths work.
Usage: table-test.py '<qemu command line of the usual tests>'."""
import re, shlex, subprocess, sys, time

cmd = sys.argv[1] + " -fw_cfg name=opt/metallium/only,string=12"
log = open("build-table.log", "w")
p = subprocess.Popen(shlex.split(cmd), stdout=log, stderr=subprocess.STDOUT)
end = time.time() + 300
text = ""
while time.time() < end:
    time.sleep(1)
    text = open("build-table.log", errors="replace").read()
    if "[table] done" in text or "PANIC" in text or "cannot map the pages" in text:
        break
p.kill()
p.wait()
text = open("build-table.log", errors="replace").read()
lines = [l for l in text.splitlines() if l.startswith("[table] TEST")]
for l in lines:
    print(l)
ok = len(lines) >= 6 and all(l.endswith("PASS") for l in lines) and "[table] done" in text
print("== TABLE: ALL PASS ==" if ok else "== TABLE: FAILED ==")
if not ok:
    print(text[-2500:])
sys.exit(0 if ok else 1)
