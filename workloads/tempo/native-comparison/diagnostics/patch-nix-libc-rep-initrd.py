#!/usr/bin/env python3
"""Relocate one REP STOSQ in the pinned Tempo initrd's Nix glibc.

Usage: patch-nix-libc-rep-initrd.py ORIGINAL_INITRD_GZ OUTPUT_INITRD_GZ
This is a workload diagnostic, not a general glibc patcher. It requires
temporary memory for the uncompressed cpio archive (about 560 MiB).
"""

import gzip
import hashlib
import struct
import sys
from pathlib import Path


ORIGINAL_INITRD_SHA256 = "97c1634a75062e4be60dbb7c7e25471971cc53c99c1a757e97558de0700ec192"
ORIGINAL_LIBC_SHA256 = "d36febd0a5e404e6bb85ada21e2c6356e2dca08f4cf60eab38cbf1bdd80a0998"
PATCHED_LIBC_SHA256 = "f4d104d3623829e181674f65a192ea9d4975ba83d2892a8514a54273efac62c6"
LIBC_NAME = b"nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/libc.so.6"
SITE = 0xAE9DC
# Page 0xa6000 has one pre-existing REP site and no faults in the baseline
# one-in-4,096 sample. Keep page 0x180000's hot two-slot guard unchanged.
CAVE = 0xA6923
SITE_ORIGINAL = bytes.fromhex("f3 48 ab 48 8d 8a 90 00 00 00")
CAVE_ORIGINAL = bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00 66 2e 0f 1f 84")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def jump(at, destination):
    displacement = destination - at - 5
    assert -(1 << 31) <= displacement < (1 << 31)
    return b"\xe9" + struct.pack("<i", displacement)


def aligned4(offset):
    return (offset + 3) & ~3


def libc_offset(archive):
    offset = 0
    found = None
    while offset + 110 <= len(archive):
        header = archive[offset : offset + 110]
        assert header[:6] in (b"070701", b"070702"), f"unexpected cpio header at {offset:#x}"
        size = int(header[54:62], 16)
        name_size = int(header[94:102], 16)
        assert name_size > 0
        name_end = offset + 110 + name_size
        assert name_end <= len(archive)
        name = archive[offset + 110 : name_end - 1]
        assert archive[name_end - 1] == 0
        start = aligned4(name_end)
        assert start + size <= len(archive)
        if name == LIBC_NAME:
            assert found is None, "duplicate libc member"
            assert size == 2413096, "unexpected libc size"
            found = (start, size)
        if name == b"TRAILER!!!":
            break
        offset = aligned4(start + size)
    assert found is not None, "pinned Nix libc not found"
    return found


def main(source, output):
    assert source.resolve() != output.resolve(), "input and output must differ"
    compressed = source.read_bytes()
    assert digest(compressed) == ORIGINAL_INITRD_SHA256, "unexpected initrd build"
    archive = bytearray(gzip.decompress(compressed))
    start, size = libc_offset(archive)
    assert digest(archive[start : start + size]) == ORIGINAL_LIBC_SHA256, "unexpected libc build"
    assert archive[start + SITE : start + SITE + len(SITE_ORIGINAL)] == SITE_ORIGINAL
    assert archive[start + CAVE : start + CAVE + len(CAVE_ORIGINAL)] == CAVE_ORIGINAL

    # Preserve REP STOSQ and LEA RCX,[RDX+0x90] unchanged, then return to the
    # following instruction. Both source and cave occupy the same ELF file.
    moved = SITE_ORIGINAL + jump(CAVE + len(SITE_ORIGINAL), SITE + len(SITE_ORIGINAL))
    assert len(moved) == len(CAVE_ORIGINAL)
    archive[start + CAVE : start + CAVE + len(moved)] = moved
    archive[start + SITE : start + SITE + len(SITE_ORIGINAL)] = (
        jump(SITE, CAVE) + b"\x90" * (len(SITE_ORIGINAL) - 5)
    )
    assert digest(archive[start : start + size]) == PATCHED_LIBC_SHA256
    print("patched libc ELF sha256:", PATCHED_LIBC_SHA256, flush=True)

    with output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", compresslevel=1, mtime=0) as zipped:
            zipped.write(archive)
    print("patched initrd.gz sha256:", digest(output.read_bytes()))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
