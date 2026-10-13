#!/usr/bin/env python3
"""Relocate one libjq REP MOVSQ in the pinned Tempo initrd (diagnostic).

Usage: relocate-libjq-rep-initrd.py ORIGINAL_INITRD_GZ OUTPUT_INITRD_GZ
This changes a pinned build only; it is not a general libjq patcher.
"""

import gzip
import hashlib
import struct
import sys
from pathlib import Path

NAME = b"nix/store/s4w1j16dj8wyriv1ljfypr9s1r39yjwp-jq-1.8.1/lib/libjq.so.1.0.4"
ORIGINAL_INITRD_SHA = "97c1634a75062e4be60dbb7c7e25471971cc53c99c1a757e97558de0700ec192"
ORIGINAL_LIBJQ_SHA = "068f5fc1a0271d9d348dc7907b9db197fbebd14b6fed8ff31d39ec11cba1ba6b"
PATCHED_LIBJQ_SHA = "e88bb54c9e66b3cfaf80c2157911061d6a99c173e1668d217ef91d4303ae2adb"
PATCHED_INITRD_SHA = "8236cca295d44362989436882e0995e45308b472355583eb4e03b123f61c2029"
SOURCE = 0x31801
SHORT_CAVE = 0x3183A
LONG_CAVE = 0x34096
SOURCE_BYTES = bytes.fromhex("f3 48 a5")
SHORT_BYTES = bytes.fromhex("66 0f 1f 44 00 00")
LONG_BYTES = bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def near_jmp(at, dest):
    return b"\xe9" + struct.pack("<i", dest - at - 5)


def main(input_path, output_path):
    assert input_path.resolve() != output_path.resolve()
    compressed = input_path.read_bytes()
    assert sha(compressed) == ORIGINAL_INITRD_SHA
    data = bytearray(gzip.decompress(compressed))
    p = 0
    found = False
    while p + 110 <= len(data):
        h = data[p:p + 110]
        assert h[:6] in (b"070701", b"070702")
        size = int(h[54:62], 16)
        name_size = int(h[94:102], 16)
        name = data[p + 110:p + 110 + name_size - 1]
        start = (p + 110 + name_size + 3) & ~3
        if name == NAME:
            assert not found and size == 440056
            found = True
            assert sha(data[start:start + size]) == ORIGINAL_LIBJQ_SHA
            assert data[start + SOURCE:start + SOURCE + 3] == SOURCE_BYTES
            assert data[start + SHORT_CAVE:start + SHORT_CAVE + 6] == SHORT_BYTES
            assert data[start + LONG_CAVE:start + LONG_CAVE + 10] == LONG_BYTES
            short_disp = SHORT_CAVE - (SOURCE + 2)
            assert -128 <= short_disp <= 127
            data[start + SOURCE:start + SOURCE + 3] = bytes([0xEB, short_disp & 0xFF, 0x90])
            data[start + SHORT_CAVE:start + SHORT_CAVE + 6] = near_jmp(SHORT_CAVE, LONG_CAVE) + b"\x90"
            data[start + LONG_CAVE:start + LONG_CAVE + 10] = SOURCE_BYTES + near_jmp(LONG_CAVE + 3, SOURCE + 3) + b"\x90\x90"
            assert sha(data[start:start + size]) == PATCHED_LIBJQ_SHA
            print("patched libjq sha256:", PATCHED_LIBJQ_SHA, flush=True)
        if name == b"TRAILER!!!":
            break
        p = (start + size + 3) & ~3
    assert found
    with output_path.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", compresslevel=1, mtime=0) as zipped:
            zipped.write(data)
    assert sha(output_path.read_bytes()) == PATCHED_INITRD_SHA
    print("patched initrd sha256:", PATCHED_INITRD_SHA)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
