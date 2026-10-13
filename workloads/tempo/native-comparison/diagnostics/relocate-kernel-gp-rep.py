#!/usr/bin/env python3
"""Move one REP MOVSQ off the hot syscall page in a pinned diagnostic kernel.

Usage: relocate-kernel-gp-rep.py VMLINUX OUTPUT_VMLINUX
Only accepts the tested guest-kernel layout; this is not a general patcher.
"""
import hashlib
import struct
import sys
from pathlib import Path

SOURCE_SHA = "5794fda70c6dac7485e4976bca515abfd25ccd5f00a05113b5574cd77acd28fc"
PATCHED_SHA = "93f151be55863fd3d27de20e9b4e778fadd794ac93f29bd3e4db7ee08bf54b33"
TEXT_FILE_OFFSET = 0x200000
SITE = 0xFB1E39
CAVE = 0xFB0EF0
ORIGINAL = bytes.fromhex("f3 48 a5 c7 44 24 68 00 00 00 00")


def jump(source, target):
    return b"\xe9" + struct.pack("<i", target - source - 5)


def main(source, output):
    assert source.resolve() != output.resolve()
    blob = bytearray(source.read_bytes())
    assert hashlib.sha256(blob).hexdigest() == SOURCE_SHA
    site = TEXT_FILE_OFFSET + SITE
    cave = TEXT_FILE_OFFSET + CAVE
    assert blob[site:site + len(ORIGINAL)] == ORIGINAL
    assert blob[cave:cave + 16] == b"\xcc" * 16
    blob[site:site + len(ORIGINAL)] = jump(SITE, CAVE) + b"\x90" * 6
    blob[cave:cave + 16] = ORIGINAL + jump(CAVE + len(ORIGINAL), SITE + len(ORIGINAL))
    assert hashlib.sha256(blob).hexdigest() == PATCHED_SHA
    output.write_bytes(blob)
    output.chmod(source.stat().st_mode)
    print("patched vmlinux:", PATCHED_SHA)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
