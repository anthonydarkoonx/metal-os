#!/usr/bin/env python3
"""Boot-time services put their mappings where the kernel chooses: compare the "[map]" lines (opt/metal/map-log=1) of
several boots. The same (role, object) pairs must be mapped every time, and the addresses must differ between boots."""
import re, sys

def load(path):
    m = {}
    for line in open(path, errors="replace"):
        g = re.match(r"\[map\] task (\d+) role (\d+) object (\d+) \((\d+) pages\) at (0x[0-9a-f]+)", line)
        if g:
            m[(int(g.group(2)), int(g.group(3)))] = int(g.group(5), 16)
    return m

boots = [load(p) for p in sys.argv[1:]]
if len(boots) < 2:
    sys.exit("need at least two boot logs")
keys = set(boots[0])
ok = len(keys) >= 20 and all(set(b) == keys for b in boots)
same = [k for k in keys if len({b.get(k) for b in boots}) < len(boots)]
# addresses are drawn from about 256 pages (small window) or a lot more: a few chance collisions are fine, many are not
ok = ok and len(same) <= max(1, len(keys) // 20)
print(f"{'ALL PASS' if ok else 'FAILED'}: {len(keys)} service mappings in {len(boots)} boots, {len(same)} with the same address every time")
sys.exit(0 if ok else 1)
