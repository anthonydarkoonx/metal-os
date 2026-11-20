#!/usr/bin/env python3
"""Boot the kernel as an ELF file (QEMU then passes no device tree) on 8 cores and check that the firmware is asked how many cores there are and all of them
come up. Usage: elf-boot-test.py '<qemu command line of the usual tests>'."""
import re, shlex, subprocess, sys

cmd = sys.argv[1]
cmd = re.sub(r"(\S+metallium-kernel)\.img", r"\1", cmd)  # the ELF, not the flat Image
ver = sys.argv[2] if len(sys.argv) > 2 else "2"  # (no tree: the kernel asks the controller which version it is)
n = "8" if ver == "2" else "32"  # (QEMU's GICv2 has up to 8 cores)
cmd = re.sub(r"gic-version=\d", f"gic-version={ver}", cmd)
cmd = re.sub(r"-smp \d+", f"-smp {n}", cmd)
log = open("build-elf.log", "w")
p = subprocess.Popen(shlex.split(cmd), stdout=log, stderr=subprocess.STDOUT)
try:
    p.wait(timeout=40)
except subprocess.TimeoutExpired:
    p.kill()
    p.wait()
out = open("build-elf.log", errors="replace").read()
ok = "[machine] no device tree" in out and f"[smp] {n} cores online" in out and f"GICv{ver}" in out and "PANIC" not in out
print(f"[elf-boot] TEST without a device tree the firmware is asked about the cores and all of them start (GICv{ver}, {n} cores):", "PASS" if ok else "FAIL")
print("== ELF BOOT: ALL PASS ==" if ok else "== ELF BOOT: FAILED ==")
if not ok:
    print(out[:1500])
sys.exit(0 if ok else 1)
