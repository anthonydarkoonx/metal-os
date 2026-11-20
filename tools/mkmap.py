#!/usr/bin/env python3
"""Build Metal Program Packages (.map) from programs/packages/<id>.manifest.

Format: see libs/metallium-map/src/lib.rs (header, manifest text, file table, data; CRC-32 over everything after the header).
Every `ability = name kind bin/X` line names a program: the package carries programs/build/X.pie as bin/X.

  mkmap.py                      build every package into programs/build/maps/<id>.map
  mkmap.py ID OUT.map           build one

Packages are signed with tools/keys/dev.seed (Ed25519), whose public key goes into the system image as /etc/program-keys.
Some manifests are deliberately flawed (negative test fixtures for the program manager):
  corrupt   a flipped byte (checksum fails)       mismatch  the id differs from the file name
  nosign    not signed                            rogue     signed with a key the system does not trust
  tampered  signed, then its data changed and the checksum recomputed (only the signature catches it)
"""
import os
import struct
import sys
import zlib
import hashlib
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..")
# A desktop (or anything else built outside this tree) can bring packages: METALLIUM_EXTRA_PACKAGES is a directory with
# packages/<id>.manifest, bin/<program>.pie and an optional `defaults` file (see docs/DESKTOP-INTERFACE.md). They are signed with the dev key.
EXTRA = os.environ.get("METALLIUM_EXTRA_PACKAGES")


def extra_defaults():
    """The text of /etc/program-defaults: the programs the image installs (and starts) by default; empty without a desktop."""
    p = os.path.join(EXTRA, "defaults") if EXTRA else None
    return open(p).read() if p and os.path.exists(p) else ""
MAGIC = b"METALMAP"


def load_key(name):
    path = os.path.join(HERE, "keys", name + ".seed")
    if not os.path.exists(path):
        # (the seeds are not in the repository: each checkout makes its own on first use, and the image trusts that dev key)
        with open(path, "w") as f:
            f.write(os.urandom(32).hex() + "\n")
        print(f"made a new signing key {path}", file=sys.stderr)
    seed = bytes.fromhex(open(path).read().strip())
    return Ed25519PrivateKey.from_private_bytes(seed)


def public_key(key):
    return key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def build(manifest: str, files, signer=None):
    """files: list of (path, bytes); signer: an Ed25519PrivateKey or None."""
    table, data = b"", b""
    for p, d in files:
        table += struct.pack("<HII", len(p), len(data), len(d)) + p.encode()
        data += d
    mb = manifest.encode()
    body = mb + table + data
    sig_len = 72 if signer else 0
    img = MAGIC + struct.pack("<IIIIII", 1, len(mb), len(files), len(table), zlib.crc32(body) & 0xFFFFFFFF, sig_len) + body
    if signer:
        img += hashlib.sha512(public_key(signer)).digest()[:8] + signer.sign(img)
    return img


def program_keys():
    """The trust store for /etc/program-keys."""
    return "# keys the system trusts to sign programs (name = public key, 64 hex digits)\ndev = %s\n" % public_key(load_key("dev")).hex()


def program_policy():
    return "# program policy: allow_unsigned = true is developer mode (unsigned packages that say `unsigned = true` may be installed)\nallow_unsigned = false\n"


def entries(manifest: str):
    out = []
    for line in manifest.splitlines():
        line = line.split("#")[0].strip()
        if line.startswith("ability"):
            parts = line.split("=", 1)[1].split()
            if len(parts) == 3 and parts[2] not in out:
                out.append(parts[2])
    return out


def package(pid: str):
    own = os.path.join(ROOT, "programs", "packages", pid + ".manifest")
    ext = os.path.join(EXTRA, "packages", pid + ".manifest") if EXTRA else None
    manifest = open(own if os.path.exists(own) else ext).read()
    files = []
    for path in entries(manifest):
        prog = os.path.basename(path)
        pie = os.path.join(ROOT, "programs", "build", prog + ".pie")
        if not os.path.exists(pie) and EXTRA:
            pie = os.path.join(EXTRA, "bin", prog + ".pie")
        files.append((path, open(pie, "rb").read()))
    signer = None if pid == "nosign" else load_key("rogue" if pid == "rogue" else "dev")
    img = build(manifest, files, signer)
    if pid == "corrupt":
        b = bytearray(img)
        b[-5 - 72] ^= 0xFF  # (a data byte: the checksum no longer matches)
        img = bytes(b)
    if pid == "tampered":
        # change a byte of the last file, fix the checksum, keep the old signature: only the signature can notice
        b = bytearray(img)
        b[-72 - 3] ^= 0x01
        body = bytes(b[32:-72])
        b[24:28] = struct.pack("<I", zlib.crc32(body) & 0xFFFFFFFF)
        img = bytes(b)
    return img


def all_ids():
    ids = set()
    for d in (os.path.join(ROOT, "programs", "packages"), os.path.join(EXTRA, "packages") if EXTRA else None):
        if d and os.path.isdir(d):
            ids |= {f[:-9] for f in os.listdir(d) if f.endswith(".manifest")}
    return sorted(ids)


def packages():
    """{file name: bytes} for the system image's /programs directory."""
    return {pid + ".map": package(pid) for pid in all_ids()}


if __name__ == "__main__":
    if len(sys.argv) == 3:
        open(sys.argv[2], "wb").write(package(sys.argv[1]))
    else:
        out = os.path.join(ROOT, "programs", "build", "maps")
        os.makedirs(out, exist_ok=True)
        for name, img in packages().items():
            open(os.path.join(out, name), "wb").write(img)
            print(f"{name}: {len(img)} bytes")
