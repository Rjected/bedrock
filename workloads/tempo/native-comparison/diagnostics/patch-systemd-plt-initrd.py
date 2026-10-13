#!/usr/bin/env python3
"""Patch only the pinned systemd payload in a Tempo initrd for paired A/B runs."""

import gzip
import hashlib
import runpy
import sys
from pathlib import Path

SOURCE_SHA = "42d0fbeba46695e99c64f6536e5c5b1545fe6ee3a8c35c77d06e79c4b86c354c"
OUTPUT_SHA = "ee78c73b7e4fcd121ec2bef13883ccbb75ebf4982ac3e2dedfda41a373f825f7"
NAME = b"nix/store/ik8a7b32nqv2q5p4g86hmp65jklmrp3g-systemd-259.3/lib/systemd/libsystemd-shared-259.so"
PATCH = Path(__file__).resolve().parents[4] / "guest/patches/relocate-systemd-plt-displacements.py"


def main(source: Path, output: Path) -> None:
    if hashlib.sha256(source.read_bytes()).hexdigest() != SOURCE_SHA:
        raise ValueError("pinned Tempo initrd changed")
    patch = runpy.run_path(str(PATCH))["patched"]
    archive = bytearray(gzip.decompress(source.read_bytes()))
    offset = 0
    found = False
    while offset + 110 <= len(archive):
        header = archive[offset:offset + 110]
        if header[:6] not in (b"070701", b"070702"):
            raise ValueError("invalid newc archive")
        size = int(header[54:62], 16)
        name_size = int(header[94:102], 16)
        name = archive[offset + 110:offset + 110 + name_size - 1]
        start = (offset + 110 + name_size + 3) & ~3
        if name == NAME:
            if found:
                raise ValueError("duplicate systemd payload")
            archive[start:start + size] = patch(bytes(archive[start:start + size]))
            found = True
        if name == b"TRAILER!!!":
            break
        offset = (start + size + 3) & ~3
    if not found:
        raise ValueError("pinned systemd payload absent")
    with output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", compresslevel=1, mtime=0) as zipped:
            zipped.write(archive)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    if digest != OUTPUT_SHA:
        raise ValueError("patched initrd changed")
    print("patched initrd:", digest)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: patch-systemd-plt-initrd.py INPUT OUTPUT")
    main(Path(sys.argv[1]), Path(sys.argv[2]))
