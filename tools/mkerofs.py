#!/usr/bin/env python3
"""Minimal EROFS image builder (uncompressed, 4 KiB blocks, compact inodes,
flat-plain layout). Written from the public format description; the Rust
reader in libs/metal-erofs is the independent check (see `make erofs-check`)."""
import struct

BLK = 4096
MAGIC = 0xE0F5E1E2
S_IFDIR, S_IFREG = 0o040000, 0o100000


def build(tree):
    """tree: {name: bytes | dict}. Returns the image as bytes."""
    nodes = []  # dicts: kind, data/children, parent

    def add(t, parent):
        nid = len(nodes)
        nodes.append({"dir": isinstance(t, dict), "t": t, "parent": parent if parent is not None else nid})
        if isinstance(t, dict):
            nodes[nid]["kids"] = []
            for name in sorted(t):
                nodes[nid]["kids"].append((name, add(t[name], nid)))
        return nid

    add(tree, None)
    meta_blocks = -(-len(nodes) * 32 // BLK)
    next_blk = 1 + meta_blocks
    for n in nodes:
        if n["dir"]:
            ents = [(".", None), ("..", None)] + n["kids"]
            blocks, cur, used = [], [], 0
            for name, kid in ents:
                need = 12 + len(name)
                if used + need > BLK:
                    blocks.append(cur); cur, used = [], 0
                cur.append((name, kid)); used += need
            blocks.append(cur)
            n["dirblocks"] = blocks
            last_used = sum(12 + len(nm) for nm, _ in blocks[-1])
            n["size"] = (len(blocks) - 1) * BLK + last_used
            n["nblocks"] = len(blocks)
        else:
            n["size"] = len(n["t"])
            n["nblocks"] = -(-n["size"] // BLK)
        n["blk"] = next_blk
        next_blk += n["nblocks"]

    img = bytearray(next_blk * BLK)
    sb = struct.pack("<IIIBBHQQIII", MAGIC, 0, 0, 12, 0, 0, len(nodes), 0, 0, next_blk, 1)
    # superblock fields: magic, csum, feature_compat, blkszbits@12, extslots@13, root_nid@14,
    # inos@16, build_time@24, build_time_nsec@32, blocks@36, meta_blkaddr@40
    img[1024:1024 + 44] = sb[:44]
    for nid, n in enumerate(nodes):
        mode = (S_IFDIR | 0o755) if n["dir"] else (S_IFREG | 0o644)
        ino = struct.pack("<HHHHIIIIHHI", 0, 0, mode, 1, n["size"], 0, n["blk"], nid, 0, 0, 0)
        img[BLK + nid * 32:BLK + nid * 32 + 32] = ino
        base = n["blk"] * BLK
        if n["dir"]:
            for bi, ents in enumerate(n["dirblocks"]):
                hdr, names = bytearray(), bytearray()
                off = 12 * len(ents)
                for name, kid in ents:
                    tgt = nid if name == "." else n["parent"] if name == ".." else kid
                    ftype = 2 if nodes[tgt]["dir"] else 1
                    hdr += struct.pack("<QHBB", tgt, off + len(names), ftype, 0)
                    names += name.encode()
                blob = bytes(hdr + names)
                img[base + bi * BLK:base + bi * BLK + len(blob)] = blob
        else:
            img[base:base + n["size"]] = n["t"]
    return bytes(img)


LOREM = "".join("Metal OS system image, line %05d: the quick brown fox\n" % i for i in range(4000)).encode()


def write_tree(tree, root):
    """Materialise a system_tree() dict as real files (for mkfs.erofs)."""
    import os
    os.makedirs(root, exist_ok=True)
    for name, t in tree.items():
        p = os.path.join(root, name)
        if isinstance(t, dict):
            write_tree(t, p)
        else:
            open(p, "wb").write(t)


def programs():
    """The programs in programs/build (position-independent ELF files, static ELF files and flat images), by file name."""
    import glob, os
    here = os.path.dirname(os.path.abspath(__file__))
    files = sorted(glob.glob(os.path.join(here, "..", "programs", "build", "*.bin")) + glob.glob(os.path.join(here, "..", "programs", "build", "*.elf")) + glob.glob(os.path.join(here, "..", "programs", "build", "*.pie")))
    # programs that are only ever installed from their package (`prog install`) and large would be left out of /bin (their image is in the package): none now
    package_only = set()
    return {os.path.basename(f) if f.endswith(".elf") else os.path.basename(f).rsplit(".", 1)[0]: open(f, "rb").read() for f in files if os.path.basename(f).rsplit(".", 1)[0] not in package_only}


def wallpapers():
    """Two stand-in wallpapers, drawn here (the same pictures as metal-desktop's demo::wallpaper_ppm), 320 x 240 binary PPM."""
    out = {}
    for name in ("dunes", "night"):
        w, h = 320, 240
        px = bytearray()
        for y2 in range(h):
            y = y2 // 2
            for x2 in range(w):
                x = x2 // 2
                if name == "night":
                    if (x * 7 + y * 13) % 97 == 0 and y < 80:
                        c = (255, 255, 230)
                    elif y > 95 + (x // 9) % 6:
                        c = (12, 14, 22)
                    else:
                        c = (10 + y // 6, 20 + y // 4, 60 + y // 2)
                else:
                    def hump(x, period, height):
                        d = (x % period) - period // 2
                        return height - (d * d) * height // ((period // 2) * (period // 2))
                    far = (124 - hump(x2 + 60, 220, 28)) // 2
                    near = (168 - hump(x2, 140, 32)) // 2
                    yy = far - 1 if (y2 // 2 < far and (y2 + 1) // 2 >= far and far > 0) else y
                    if yy < far:
                        c = (120 + yy // 2, 165 + yy // 3, 215 + yy // 8)
                    elif yy < near:
                        s = min(yy - far, 30)
                        c = (215 - s, 160 - s // 2, 105 - s // 3)
                    else:
                        s = min((yy - near) * 2, 70)
                        c = (240 - s, 180 - s // 2, 110 - s // 3)
                px += bytes(c)
        out[name + ".ppm"] = b"P6\n# Metal OS stand-in\n%d %d\n255\n" % (w, h) + bytes(px)
    return out


def system_tree():
    tree = {
        "hello.txt": b"Hello from the Metal OS system image!\n",
        "etc": {"os-release": b"NAME=MetalOS\nVERSION=0.1\n"},
        "bin": {"blob.bin": bytes((i * 7) % 251 for i in range(9000))},
        # compressible: exercises the LZ4 path when the image is built with mkfs.erofs -zlz4hc
        "usr": {"share": {"lorem.txt": LOREM}},
    }
    tree["bin"].update(programs())
    import mkmap
    tree["etc"]["program-keys"] = mkmap.program_keys().encode()
    tree["etc"]["program-policy"] = mkmap.program_policy().encode()
    tree["usr"]["programs"] = mkmap.packages()  # Metal Program Packages for the program manager (see docs/PROGRAMS.md)
    tree["usr"]["share"]["wallpapers"] = wallpapers()  # the desktop's (../docs/DESKTOP.md §10.5)
    return tree
