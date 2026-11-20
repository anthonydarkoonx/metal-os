#!/usr/bin/env python3
"""Boot the kernel as an ELF file (QEMU then passes no device tree) on 8 cores and check that the firmware is asked how many cores there are and all of them
come up. Usage: elf-boot-test.py '<qemu command line of the usual tests>'."""
import re, shlex, subprocess, sys

cmd = sys.argv[1]
cmd = re.sub(r"(\S+metal-kernel)\.img", r"\1", cmd)  # the ELF, not the flat Image
cmd = re.sub(r"-smp \d+", "-smp 8", cmd)
cmd = re.sub(r"gic-version=\d", "gic-version=2", cmd)  # (no tree: the kernel assumes a GICv2, and QEMU's GICv2 has up to 8 cores)
log = open("build-elf.log", "w")
p = subprocess.Popen(shlex.split(cmd), stdout=log, stderr=subprocess.STDOUT)
try:
    p.wait(timeout=40)
except subprocess.TimeoutExpired:
    p.kill()
    p.wait()
out = open("build-elf.log", errors="replace").read()
ok = "[machine] no device tree" in out and "[smp] 8 cores online" in out and "PANIC" not in out
print("[elf-boot] TEST without a device tree the firmware is asked about the cores and all 8 start:", "PASS" if ok else "FAIL")
print("== ELF BOOT: ALL PASS ==" if ok else "== ELF BOOT: FAILED ==")
if not ok:
    print(out[:1500])
sys.exit(0 if ok else 1)
