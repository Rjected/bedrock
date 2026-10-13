#!/usr/bin/env python3
"""Experimental exact-build rewrite of five ZSTD REP MOVSQ sites in Tempo OCI.

Usage: patch-zstd-rep-oci.py ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR
Requires temporary disk space for an uncompressed ~1.2 GB layer.
"""
import gzip
import hashlib
import io
import json
import shutil
import struct
import sys
import tarfile
import tempfile
from pathlib import Path

ORIGINAL_TEMPO_SHA256 = "1b290045dad0d18874759945281c2ccb5dee4a042570f7ffda8d135c4476ec92"
PATCHED_TEMPO_SHA256 = "52f6ee12b65d979bf40426493a589987fd8b3fa126b9edfb2296d71d3df5c829"
TEXT_BIAS = 0x1000
CAVES = [(0x7A803CD, 0x7A80400), (0x7A767CE, 0x7A76800), (0x7B1C041, 0x7B1C080)]
# Patch address, stolen instructions, instructions before/after the copy loop, cave index.
# The CMP at 0x7a9b7b1 is also a branch target, so its site steals the preceding MOV.
SITES = [
    (0x7A9B3E7, bytes.fromhex("f3 48 a5 85 c0"), b"", bytes.fromhex("85 c0"), 0),
    (0x7A9B5F4, bytes.fromhex("f3 48 a5 48 83 fb 1f"), b"", bytes.fromhex("48 83 fb 1f"), 0),
    (0x7A9B4C5, bytes.fromhex("f3 48 a5 85 c0"), b"", bytes.fromhex("85 c0"), 1),
    (0x7A9B7AB, bytes.fromhex("44 89 e1 f3 48 a5"), bytes.fromhex("44 89 e1"), b"", 1),
    (0x7A9B81B, bytes.fromhex("f3 48 a5 48 83 fb 1f"), b"", bytes.fromhex("48 83 fb 1f"), 2),
]
LOOP = bytes.fromhex("48 85 c9 74 07 48 a5 48 ff c9 75 f9")


def sha256(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def file_sha256(path, offset=0, length=None):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        source.seek(offset)
        while length is None or length > 0:
            chunk = source.read(min(8 << 20, length) if length is not None else 8 << 20)
            if not chunk:
                break
            digest.update(chunk)
            if length is not None:
                length -= len(chunk)
    return digest.hexdigest()


def blob_path(digest):
    return "blobs/sha256/" + digest.split(":", 1)[1]


def encoded(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def jump(at, target):
    delta = target - at - 5
    assert -(1 << 31) <= delta < 1 << 31
    return b"\xe9" + struct.pack("<i", delta)


def patch_layer(layer_path):
    with tarfile.open(layer_path) as layer:
        member = layer.getmember("usr/local/bin/tempo")
        assert member.size == 1163218968, "unexpected Tempo ELF size"
        base = member.offset_data
    assert file_sha256(layer_path, base, member.size) == ORIGINAL_TEMPO_SHA256, "unexpected Tempo build"
    with open(layer_path, "r+b") as layer:
        for start, end in CAVES:
            layer.seek(base + start - TEXT_BIAS)
            assert layer.read(end - start) == bytes(end - start), "code cave occupied"
        cursors = [start for start, _ in CAVES]
        for address, original, before, after, cave_index in SITES:
            layer.seek(base + address - TEXT_BIAS)
            assert layer.read(len(original)) == original, f"unexpected bytes at {address:#x}"
            cave = cursors[cave_index]
            code = before + LOOP + after + jump(cave + len(before) + len(LOOP) + len(after), address + len(original))
            assert cave + len(code) <= CAVES[cave_index][1], "code cave full"
            layer.seek(base + cave - TEXT_BIAS)
            layer.write(code)
            layer.seek(base + address - TEXT_BIAS)
            layer.write(jump(address, cave) + b"\x90" * (len(original) - 5))
            cursors[cave_index] += len(code)
    return base, member.size


def main(source, output):
    assert source.resolve() != output.resolve(), "output must differ from input"
    with tempfile.TemporaryDirectory(prefix="bedrock-zstd-") as temporary:
        layer_path = Path(temporary) / "layer.tar"
        compressed_path = Path(temporary) / "layer.tar.gz"
        with tarfile.open(source) as archive:
            top = json.load(archive.extractfile("index.json"))
            nested_descriptor = top["manifests"][0]
            nested = json.load(archive.extractfile(blob_path(nested_descriptor["digest"])))
            amd_descriptor = next(item for item in nested["manifests"] if item.get("platform", {}).get("architecture") == "amd64")
            manifest = json.load(archive.extractfile(blob_path(amd_descriptor["digest"])))
            config = json.load(archive.extractfile(blob_path(manifest["config"]["digest"])))
            old_layer = blob_path(manifest["layers"][3]["digest"])
            classic = json.load(archive.extractfile("manifest.json"))
            with gzip.GzipFile(fileobj=archive.extractfile(old_layer), mode="rb") as raw, open(layer_path, "wb") as unpacked:
                shutil.copyfileobj(raw, unpacked, 8 << 20)
        base, length = patch_layer(layer_path)
        patched_digest = file_sha256(layer_path, base, length)
        assert patched_digest == PATCHED_TEMPO_SHA256, "unexpected patch output"
        print("patched Tempo ELF sha256:", patched_digest, flush=True)
        config["rootfs"]["diff_ids"][3] = "sha256:" + file_sha256(layer_path)
        with open(layer_path, "rb") as raw, open(compressed_path, "wb") as compressed_file:
            with gzip.GzipFile(filename="", mode="wb", fileobj=compressed_file, compresslevel=1, mtime=0) as compressed:
                shutil.copyfileobj(raw, compressed, 8 << 20)
        new_layer_digest = "sha256:" + file_sha256(compressed_path)
        manifest["layers"][3].update(digest=new_layer_digest, size=compressed_path.stat().st_size)
        config_bytes = encoded(config)
        manifest["config"].update(digest=sha256(config_bytes), size=len(config_bytes))
        manifest_bytes = encoded(manifest)
        amd_descriptor.update(digest=sha256(manifest_bytes), size=len(manifest_bytes))
        nested_bytes = encoded(nested)
        nested_descriptor.update(digest=sha256(nested_bytes), size=len(nested_bytes))
        classic[0]["Config"] = blob_path(manifest["config"]["digest"])
        classic[0]["Layers"][3] = blob_path(new_layer_digest)
        replacements = {
            "index.json": encoded(top),
            "manifest.json": encoded(classic),
            blob_path(new_layer_digest): compressed_path,
            blob_path(manifest["config"]["digest"]): config_bytes,
            blob_path(amd_descriptor["digest"]): manifest_bytes,
            blob_path(nested_descriptor["digest"]): nested_bytes,
        }
        with tarfile.open(source) as archive, tarfile.open(output, "w") as result:
            for member in archive:
                if member.name in ("index.json", "manifest.json", old_layer):
                    continue
                result.addfile(member, archive.extractfile(member) if member.isfile() else None)
            for name, value in replacements.items():
                member = tarfile.TarInfo(name)
                member.mode = 0o644
                if isinstance(value, Path):
                    member.size = value.stat().st_size
                    with open(value, "rb") as content:
                        result.addfile(member, content)
                else:
                    member.size = len(value)
                    result.addfile(member, io.BytesIO(value))
    print("patched OCI archive sha256:", file_sha256(output))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
