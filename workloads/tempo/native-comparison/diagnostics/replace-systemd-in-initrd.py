#!/usr/bin/env python3
"""Replace the pinned systemd shared library in a newc Tempo initrd.

This is a diagnostic image builder. It preserves every archive entry and its
metadata except the target entry's size and contents.
"""

import gzip
import hashlib
import sys
from pathlib import Path


SOURCE_SHA = "42d0fbeba46695e99c64f6536e5c5b1545fe6ee3a8c35c77d06e79c4b86c354c"
ORIGINAL_SHA = "1bf501425769972af2fea581bd6f1dd16d4f7a494d926c399a3b5edeb8f95a5d"
NAME = b"nix/store/ik8a7b32nqv2q5p4g86hmp65jklmrp3g-systemd-259.3/lib/systemd/libsystemd-shared-259.so"


def align4(data: bytes) -> bytes:
    return data + b"\0" * (-len(data) % 4)


def replace(source: Path, binary: Path, output: Path) -> None:
    if hashlib.sha256(source.read_bytes()).hexdigest() != SOURCE_SHA:
        raise ValueError("pinned Tempo initrd changed")
    archive = gzip.decompress(source.read_bytes())
    replacement = binary.read_bytes()
    chunks = []
    offset = 0
    found = False
    while offset + 110 <= len(archive):
        header = archive[offset:offset + 110]
        if header[:6] != b"070701":
            raise ValueError(f"invalid newc archive at {offset}")
        size = int(header[54:62], 16)
        name_size = int(header[94:102], 16)
        name_end = offset + 110 + name_size
        name = archive[offset + 110:name_end - 1]
        start = (name_end + 3) & ~3
        data = archive[start:start + size]
        if name == NAME:
            if found:
                raise ValueError("duplicate systemd payload")
            if hashlib.sha256(data).hexdigest() != ORIGINAL_SHA:
                raise ValueError("pinned systemd payload changed")
            header = header[:54] + f"{len(replacement):08x}".encode() + header[62:]
            data = replacement
            found = True
        chunks.extend((header, archive[offset + 110:start], align4(data)))
        offset = (start + size + 3) & ~3
        if name == b"TRAILER!!!":
            break
    if not found:
        raise ValueError("pinned systemd payload absent")
    chunks.append(archive[offset:])
    with output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", compresslevel=1, mtime=0) as zipped:
            zipped.write(b"".join(chunks))
    print(hashlib.sha256(output.read_bytes()).hexdigest(), output)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("usage: replace-systemd-in-initrd.py SOURCE LIBSYSTEMD OUTPUT")
    replace(*(Path(path) for path in sys.argv[1:]))
