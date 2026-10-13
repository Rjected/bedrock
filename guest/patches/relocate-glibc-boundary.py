#!/usr/bin/env python3
"""Relocate one page-spanning PADDB in the pinned guest glibc build.

The original instruction in __strncasecmp_l_sse2 starts at file offset
0xc2ffd. Its 0x0f opcode byte is the final byte of the 4 KiB page, forcing
Bedrock to guard that otherwise hot page. This exact-build transform moves
the instruction into unused alignment padding and jumps back afterward.
"""

import hashlib
import stat
import struct
import sys
from pathlib import Path

SOURCE_SHA = "d36febd0a5e404e6bb85ada21e2c6356e2dca08f4cf60eab38cbf1bdd80a0998"
OUTPUT_SHA = "18737a240f54f96bcca61ebc69df81d8702ee3d587546f423eb10665eb8eaf59"
SITE = 0xC2FFD
# Twelve padding bytes after an unconditional jump in the same function and
# unwind FDE. Keeping the trampoline here also avoids a cross-function jump.
CAVE = 0xC2FB4
SITE_BYTES = bytes.fromhex("66 41 0f fc c8")
CAVE_BYTES = bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00 90")


def jump(source: int, target: int) -> bytes:
    return b"\xe9" + struct.pack("<i", target - source - 5)


def patch(source: Path, output: Path) -> None:
    blob = bytearray(source.read_bytes())
    if hashlib.sha256(blob).hexdigest() != SOURCE_SHA:
        raise ValueError("guest glibc build changed; review the boundary patch")
    if blob[SITE:SITE + len(SITE_BYTES)] != SITE_BYTES:
        raise ValueError("guest glibc instruction bytes changed")
    if blob[CAVE:CAVE + len(CAVE_BYTES)] != CAVE_BYTES:
        raise ValueError("guest glibc padding bytes changed")

    blob[SITE:SITE + len(SITE_BYTES)] = jump(SITE, CAVE)
    blob[CAVE:CAVE + len(CAVE_BYTES)] = (
        SITE_BYTES + jump(CAVE + len(SITE_BYTES), SITE + len(SITE_BYTES)) + b"\x90" * 2
    )
    if hashlib.sha256(blob).hexdigest() != OUTPUT_SHA:
        raise ValueError("guest glibc patched hash changed")

    mode = stat.S_IMODE(source.stat().st_mode)
    if source == output:
        output.chmod(mode | stat.S_IWUSR)
    output.write_bytes(blob)
    output.chmod(mode)
    print(f"AMD guest glibc boundary patch: {OUTPUT_SHA}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: relocate-glibc-boundary.py INPUT_LIBC OUTPUT_LIBC")
    patch(Path(sys.argv[1]), Path(sys.argv[2]))
