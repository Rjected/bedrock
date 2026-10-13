#!/usr/bin/env python3
"""Experimental exact-build relocation of eight glibc REP sites in Tempo OCI.

Usage: relocate-libc-rep-reuse-oci.py ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR
Requires temporary disk space for an uncompressed Tempo base layer.
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

ORIGINAL_LIBC_SHA256 = "bff8750fe719e6000791b88b11747dce8772c37118d0b2348044b70819d13835"
PATCHED_LIBC_SHA256 = "69337a8d23ada58081425160eb0a7c7143520cad53c42231033205ad420c64d3"
LIBC_PATH = "usr/lib/x86_64-linux-gnu/libc.so.6"
LAYER_INDEX = 0
# Preserve each REP instruction and its following complete instruction in
# alignment padding after RET or an unconditional jump. The destination pages
# already contain hazards; each has at most four after this relocation.
SITES = [
    (0x152C64, 0x15A982, bytes.fromhex("f3 a4 c4 c1 7e 7f 00")),
    (0x152CCA, 0x15A9A0, bytes.fromhex("f3 a4 c4 c1 7e 7f 00")),
    (0x3C40A, 0x983F6, bytes.fromhex("f3 48 ab 31 ff")),
    (0x3C48A, 0x98FE1, bytes.fromhex("f3 48 ab bf 02 00 00 00")),
    (0x3C50A, 0x98912, bytes.fromhex("f3 48 ab 0f 1f 00")),
    (0x3C5A2, 0x96063, bytes.fromhex("f3 48 ab 48 89 f7")),
    (0x3C6CE, 0x15A98E, bytes.fromhex("f3 48 ab 89 94 24 28 01 00 00")),
    (0x3CD36, 0x15A9AC, bytes.fromhex("f3 48 ab 89 34 24")),
]
CAVE_ORIGINAL = {
    0x15A982: bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00 66 2e"),
    0x15A9A0: bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00 66 2e"),
    0x983F6: bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00"),
    0x98FE1: bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00 0f 1f"),
    0x98912: bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00"),
    0x96063: bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00"),
    0x15A98E: bytes.fromhex("0f 1f 84 00 00 00 00 00 66 2e 0f 1f 84 00 00"),
    0x15A9AC: bytes.fromhex("0f 1f 84 00 00 00 00 00 66 2e 0f"),
}


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
        member = layer.getmember(LIBC_PATH)
        assert member.size == 1926232, "unexpected libc ELF size"
        base = member.offset_data
    assert file_sha256(layer_path, base, member.size) == ORIGINAL_LIBC_SHA256, "unexpected libc build"
    with open(layer_path, "r+b") as layer:
        for address, cave, original in SITES:
            layer.seek(base + address)
            assert layer.read(len(original)) == original, f"unexpected bytes at {address:#x}"
            layer.seek(base + cave)
            assert layer.read(len(CAVE_ORIGINAL[cave])) == CAVE_ORIGINAL[cave], f"code cave changed at {cave:#x}"
            code = original + jump(cave + len(original), address + len(original))
            assert len(code) == len(CAVE_ORIGINAL[cave])
            layer.seek(base + cave)
            layer.write(code)
            layer.seek(base + address)
            layer.write(jump(address, cave) + b"\x90" * (len(original) - 5))
    return base, member.size


def main(source, output):
    assert source.resolve() != output.resolve(), "output must differ from input"
    with tempfile.TemporaryDirectory(prefix="bedrock-libc-rep-") as temporary:
        layer_path = Path(temporary) / "layer.tar"
        compressed_path = Path(temporary) / "layer.tar.gz"
        with tarfile.open(source) as archive:
            top = json.load(archive.extractfile("index.json"))
            nested_descriptor = top["manifests"][0]
            nested = json.load(archive.extractfile(blob_path(nested_descriptor["digest"])))
            amd_descriptor = next(item for item in nested["manifests"] if item.get("platform", {}).get("architecture") == "amd64")
            manifest = json.load(archive.extractfile(blob_path(amd_descriptor["digest"])))
            config = json.load(archive.extractfile(blob_path(manifest["config"]["digest"])))
            old_layer = blob_path(manifest["layers"][LAYER_INDEX]["digest"])
            classic = json.load(archive.extractfile("manifest.json"))
            with gzip.GzipFile(fileobj=archive.extractfile(old_layer), mode="rb") as raw, open(layer_path, "wb") as unpacked:
                shutil.copyfileobj(raw, unpacked, 8 << 20)
        base, length = patch_layer(layer_path)
        patched_digest = file_sha256(layer_path, base, length)
        assert patched_digest == PATCHED_LIBC_SHA256, "unexpected patch output"
        print("patched libc ELF sha256:", patched_digest, flush=True)
        config["rootfs"]["diff_ids"][LAYER_INDEX] = "sha256:" + file_sha256(layer_path)
        with open(layer_path, "rb") as raw, open(compressed_path, "wb") as compressed_file:
            with gzip.GzipFile(filename="", mode="wb", fileobj=compressed_file, compresslevel=1, mtime=0) as compressed:
                shutil.copyfileobj(raw, compressed, 8 << 20)
        new_layer_digest = "sha256:" + file_sha256(compressed_path)
        manifest["layers"][LAYER_INDEX].update(digest=new_layer_digest, size=compressed_path.stat().st_size)
        config_bytes = encoded(config)
        manifest["config"].update(digest=sha256(config_bytes), size=len(config_bytes))
        manifest_bytes = encoded(manifest)
        amd_descriptor.update(digest=sha256(manifest_bytes), size=len(manifest_bytes))
        nested_bytes = encoded(nested)
        nested_descriptor.update(digest=sha256(nested_bytes), size=len(nested_bytes))
        classic[0]["Config"] = blob_path(manifest["config"]["digest"])
        classic[0]["Layers"][LAYER_INDEX] = blob_path(new_layer_digest)
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
