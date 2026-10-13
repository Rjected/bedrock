#!/usr/bin/env python3
"""Move a pinned Nix libc cross-page PADDB into nearby padding.

Usage: relocate-nix-libc-boundary-initrd.py LIBJQ_PATCHED_INITRD_GZ OUTPUT_INITRD_GZ
This exact-build diagnostic requires the preceding libjq relocation. It is
not a general glibc patcher.
"""
import gzip
import hashlib
import struct
import sys
from pathlib import Path

NAME = b"nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/libc.so.6"
SOURCE_SHA = "8236cca295d44362989436882e0995e45308b472355583eb4e03b123f61c2029"
LIBC_SHA = "d36febd0a5e404e6bb85ada21e2c6356e2dca08f4cf60eab38cbf1bdd80a0998"
PATCHED_LIBC_SHA = "59b2f6ed8a1e4f6b5310e42f4bbd45fb52e8c42737a5f52a243330863f1571ed"
PATCHED_INITRD_SHA = "cf1632c3409f3d709cb5be5d2a7501f86201508af4ec68d660d268ddae2f70c4"
SITE = 0xC2FFD
CAVE = 0xC21A2
SITE_BYTES = bytes.fromhex("66 41 0f fc c8")
CAVE_BYTES = bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00 0f 1f 00")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def jump(at, destination):
    return b"\xe9" + struct.pack("<i", destination - at - 5)


def main(source, output):
    assert source.resolve() != output.resolve()
    compressed = source.read_bytes()
    assert sha(compressed) == SOURCE_SHA
    archive = bytearray(gzip.decompress(compressed))
    p = 0
    found = False
    while p + 110 <= len(archive):
        header = archive[p:p + 110]
        assert header[:6] in (b"070701", b"070702")
        size = int(header[54:62], 16)
        name_size = int(header[94:102], 16)
        name = archive[p + 110:p + 110 + name_size - 1]
        start = (p + 110 + name_size + 3) & ~3
        if name == NAME:
            assert not found and size == 2413096
            found = True
            assert sha(archive[start:start + size]) == LIBC_SHA
            assert archive[start + SITE:start + SITE + 5] == SITE_BYTES
            assert archive[start + CAVE:start + CAVE + 14] == CAVE_BYTES
            archive[start + SITE:start + SITE + 5] = jump(SITE, CAVE)
            archive[start + CAVE:start + CAVE + 14] = (
                SITE_BYTES + jump(CAVE + 5, SITE + 5) + b"\x90" * 4
            )
            assert sha(archive[start:start + size]) == PATCHED_LIBC_SHA
            print("patched libc:", PATCHED_LIBC_SHA, flush=True)
        if name == b"TRAILER!!!":
            break
        p = (start + size + 3) & ~3
    assert found
    with output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", compresslevel=1, mtime=0) as zipped:
            zipped.write(archive)
    assert sha(output.read_bytes()) == PATCHED_INITRD_SHA
    print("patched initrd:", PATCHED_INITRD_SHA)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
