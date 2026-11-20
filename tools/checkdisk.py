#!/usr/bin/env python3
"""After the QEMU run, check what the guest left on the disk (host-side view)."""
import struct
import sys

f = open(sys.argv[1], "rb")  # (a disk can be large and sparse: read only the sectors that are looked at)
def sec(s, n=1):
    f.seek(s * 512)
    return f.read(n * 512)

for s in range(100, 108):
    want = bytes((s * 31 + i) & 0xFF for i in range(512))
    if sec(s) != want:
        sys.exit(f"disk check FAILED: scratch sector {s} wrong")
if struct.unpack_from("<I", sec(1024 + 2), 0)[0] != 0xE0F5E1E2:  # superblock at byte 1024
    sys.exit("disk check FAILED: system partition clobbered")
LFS = 13312
if sec(LFS, 8)[:8] == b"MTLMVOL1":
    # an encrypted volume: its checkpoints are ciphertext; what can be checked from outside is that the header is there and nothing readable is on the volume
    body = sec(LFS + 8, 9 * 8)
    if b"MTLMCKPT" in body or struct.pack("<I", 0x4D434B50) in body[:4]:
        sys.exit("disk check FAILED: a plain checkpoint on an encrypted volume")
    print("host-side disk check: scratch data and EROFS partition intact; the data partition is an encrypted volume (header present, first store blocks are not plain checkpoints)")
    sys.exit(0)
seqs = []
for slot in (0, 1):
    f.seek(LFS * 512 + slot * 4096)
    cp = f.read(4096)
    if struct.unpack_from("<I", cp, 0)[0] == 0x4D434B50:
        seqs.append(struct.unpack_from("<Q", cp, 4)[0])
if not seqs:
    sys.exit("disk check FAILED: no LFS checkpoint on the data partition")
print(f"host-side disk check: scratch data, EROFS partition and LFS checkpoints (seq {max(seqs)}) all intact")
