#!/usr/bin/env python3
"""Experimental exact-build relocation of four glibc REP sites in txgen OCI.

Usage: relocate-txgen-libc-rep-four-oci.py ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR
Requires temporary disk space for an uncompressed txgen base layer.
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

ORIGINAL_LIBC_SHA256 = "6b4a45352fd0c540a9c7c718f35ce8c8e46a4e482f9d3885a910c32d1a0e1421"
PATCHED_LIBC_SHA256 = "c2363d4b93e39ad14de395dd8497da76e1474b2260807a01cf5dba4d3352a6d4"
LIBC_PATH = "usr/lib/x86_64-linux-gnu/libc.so.6"
LAYER_INDEX = 0
IMAGE_INDEX = 2
# Put the REP operations into alignment padding after a RET on file page
# 0x15a000. The final STOSQ site ends in a relative jump, which is retargeted
# to its original destination from the code cave.
SITES = [
    (0x152D24, 0x15AA42, bytes.fromhex("f3 a4 c4 c1 7e 7f 00"), bytes.fromhex("f3 a4 c4 c1 7e 7f 00"), 0x152D2B),
    (0x152D8A, 0x15AA4E, bytes.fromhex("f3 a4 c4 c1 7e 7f 00"), bytes.fromhex("f3 a4 c4 c1 7e 7f 00"), 0x152D91),
    (0x9828E, 0x15AA5A, bytes.fromhex("f3 48 ab 48 83 c4 08"), bytes.fromhex("f3 48 ab 48 83 c4 08"), 0x98295),
    (0x96785, 0x15AA66, bytes.fromhex("f3 48 ab e9 80 fa ff ff"), bytes.fromhex("f3 48 ab"), 0x9620D),
]
CAVE_ORIGINAL = {
    0x15AA42: bytes.fromhex("66 2e 0f 1f 84 00 00 00 00 00 66 2e"),
    0x15AA4E: bytes.fromhex("0f 1f 84 00 00 00 00 00 66 2e 0f 1f"),
    0x15AA5A: bytes.fromhex("84 00 00 00 00 00 66 2e 0f 1f 84 00"),
    0x15AA66: bytes.fromhex("00 00 00 00 66 2e 0f 1f"),
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
        for address, cave, original, moved, continuation in SITES:
            layer.seek(base + address)
            assert layer.read(len(original)) == original, f"unexpected bytes at {address:#x}"
            layer.seek(base + cave)
            assert layer.read(len(CAVE_ORIGINAL[cave])) == CAVE_ORIGINAL[cave], f"code cave changed at {cave:#x}"
            code = moved + jump(cave + len(moved), continuation)
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
            nested_descriptor = top["manifests"][IMAGE_INDEX]
            assert nested_descriptor.get("annotations", {}).get("io.containerd.image.name") == "docker.io/bedrock/tempo-txgen:latest"
            nested = json.load(archive.extractfile(blob_path(nested_descriptor["digest"])))
            amd_descriptor = next(item for item in nested["manifests"] if item.get("platform", {}).get("architecture") == "amd64")
            manifest = json.load(archive.extractfile(blob_path(amd_descriptor["digest"])))
            config = json.load(archive.extractfile(blob_path(manifest["config"]["digest"])))
            old_layer = blob_path(manifest["layers"][LAYER_INDEX]["digest"])
            classic = json.load(archive.extractfile("manifest.json"))
            assert classic[IMAGE_INDEX]["RepoTags"] == ["bedrock/tempo-txgen:latest"]
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
        classic[IMAGE_INDEX]["Config"] = blob_path(manifest["config"]["digest"])
        classic[IMAGE_INDEX]["Layers"][LAYER_INDEX] = blob_path(new_layer_digest)
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
