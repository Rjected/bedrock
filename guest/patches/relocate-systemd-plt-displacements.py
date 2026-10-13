#!/usr/bin/env python3
"""Move false REP bytes in the pinned systemd PLT off two hot code pages.

Twelve lazy PLT entries have an F2 byte inside their RIP displacement. Bedrock
must conservatively treat that byte as a possible REP prefix at an alternate
entry point. Each rewritten entry jumps to an indirect JMP in unreachable
alignment padding. The original PLT +6 lazy-binding PUSH/JMP path and GOT
slot stay at their original addresses.
"""

import hashlib
import stat
import struct
import sys
from pathlib import Path

SOURCE_SHA = "1bf501425769972af2fea581bd6f1dd16d4f7a494d926c399a3b5edeb8f95a5d"
OUTPUT_SHA = "1d32543f49e3444a2a1c80622df4a6cc59c2dfd21bab4b6ef2e4e8b30fd28c14"

SITES = [
    0xB5170, 0xB5370, 0xB5570, 0xB5770,
    0xB5970, 0xB5B70, 0xB5D70, 0xB5F70,
    0xB6170, 0xB6370, 0xB6570, 0xB6770,
]
# Every cave is an entire NOP after a RET or unconditional JMP, within a
# page with no existing hazardous instruction bytes. The source hash pins
# those facts to this exact systemd build.
CAVES = [
    (0xD0129, 7), (0xD05B7, 9), (0xD0617, 9), (0xD0636, 10),
    (0xD07A7, 9), (0xD0A57, 9), (0xD0BE1, 7), (0xD0C91, 7),
    (0xD6052, 6), (0xD6263, 11), (0xD633A, 6), (0xD63E2, 6),
]
NOP_PREFIXES = (b"\x90", b"\x66\x90", b"\x0f\x1f", b"\x66\x0f\x1f", b"\x66\x2e\x0f\x1f", b"\x66\x66\x2e\x0f\x1f")


def rel32(source: int, length: int, target: int) -> bytes:
    return struct.pack("<i", target - source - length)


def patched(source: bytes) -> bytes:
    if hashlib.sha256(source).hexdigest() != SOURCE_SHA:
        raise ValueError("guest systemd build changed; review the PLT patch")
    blob = bytearray(source)
    for site, (cave, length) in zip(SITES, CAVES, strict=True):
        old = bytes(blob[site:site + 6])
        if old[:3] != b"\xff\x25\xf2":
            raise ValueError(f"PLT entry at {site:#x} changed")
        original_got = site + 6 + struct.unpack("<i", old[2:6])[0]
        original_pad = bytes(blob[cave:cave + length])
        if not original_pad.startswith(NOP_PREFIXES):
            raise ValueError(f"alignment cave at {cave:#x} changed")
        # JMP rel32; NOP. The lazy resolver still lands at site + 6.
        blob[site:site + 6] = b"\xe9" + rel32(site, 5, cave) + b"\x90"
        # JMP qword ptr [RIP + disp32] to the original GOT slot.
        blob[cave:cave + length] = (
            b"\xff\x25" + rel32(cave, 6, original_got) + b"\x90" * (length - 6)
        )
        new_got = cave + 6 + struct.unpack("<i", blob[cave + 2:cave + 6])[0]
        if new_got != original_got or blob[site + 6:site + 7] != b"\x68":
            raise ValueError("PLT target or lazy-binding path changed")
    return bytes(blob)


def patch(source: Path, output: Path) -> None:
    result = patched(source.read_bytes())
    digest = hashlib.sha256(result).hexdigest()
    if digest != OUTPUT_SHA:
        raise ValueError("patched systemd output changed")
    mode = stat.S_IMODE(source.stat().st_mode)
    if source == output:
        output.chmod(mode | stat.S_IWUSR)
    output.write_bytes(result)
    output.chmod(mode)
    print(f"AMD guest systemd PLT patch: {digest}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: relocate-systemd-plt-displacements.py INPUT OUTPUT")
    patch(Path(sys.argv[1]), Path(sys.argv[2]))
