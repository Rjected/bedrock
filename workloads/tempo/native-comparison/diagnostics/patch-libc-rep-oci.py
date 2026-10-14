#!/usr/bin/env python3
"""Diagnostic-only rewrite of two REP MOVSB sites in the pinned Tempo libc.

Usage: patch-libc-rep-oci.py ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR [loop|relocate]
This changes guest code and cannot establish a general hypervisor speedup.
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
LIBC_MEMBER = "usr/lib/x86_64-linux-gnu/libc.so.6"
CAVES = {
    "loop": (0x152989, 0x1529c0, bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00") * 5),
    "relocate": (0x163e19, 0x163e40,
                 bytes.fromhex("66 66 2e 0f 1f 84 00 00 00 00 00") * 3
                 + bytes.fromhex("66 0f 1f 44 00 00")),
}
SITES = (0x152c64, 0x152cca)
STOLEN = bytes.fromhex("f3 a4 c4 c1 7e 7f 00")
# JRCXZ skips an empty copy; MOVSB and LOOP preserve flags and DF.
LOOP = bytes.fromhex("e3 03 a4 e2 fd")


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


def patch_layer(layer_path, mode):
    with tarfile.open(layer_path) as layer:
        member = layer.getmember(LIBC_MEMBER)
        assert member.size == 1926232, "unexpected libc ELF size"
        base = member.offset_data
    assert file_sha256(layer_path, base, member.size) == ORIGINAL_LIBC_SHA256, "unexpected libc build"
    with open(layer_path, "r+b") as layer:
        cave_start, cave_end, original_cave = CAVES[mode]
        layer.seek(base + cave_start)
        assert layer.read(cave_end - cave_start) == original_cave, "alignment cave changed"
        cave = cave_start + 5
        for address in SITES:
            layer.seek(base + address)
            assert layer.read(len(STOLEN)) == STOLEN, f"unexpected bytes at {address:#x}"
            body = (LOOP + STOLEN[2:]) if mode == "loop" else STOLEN
            code = body + jump(cave + len(body), address + len(STOLEN))
            assert cave + len(code) <= cave_end, "alignment cave full"
            layer.seek(base + cave)
            layer.write(code)
            layer.seek(base + address)
            layer.write(jump(address, cave) + b"\x90" * (len(STOLEN) - 5))
            cave += len(code)
        layer.seek(base + cave_start)
        layer.write(jump(cave_start, cave_end))
    return base, member.size


def main(source, output, mode):
    assert source.resolve() != output.resolve(), "output must differ from input"
    assert mode in CAVES, "unknown diagnostic mode"
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="bedrock-libc-") as temporary:
        layer_path = Path(temporary) / "layer.tar"
        compressed_path = Path(temporary) / "layer.tar.gz"
        with tarfile.open(source) as archive:
            top = json.load(archive.extractfile("index.json"))
            old_layer = "blobs/sha256/da539b6761059a0a114c6671f1267b57445e3a54da023db5c28be019e40f0284"
            classic = json.load(archive.extractfile("manifest.json"))
            with gzip.GzipFile(fileobj=archive.extractfile(old_layer), mode="rb") as raw, open(layer_path, "wb") as unpacked:
                shutil.copyfileobj(raw, unpacked, 8 << 20)
        base, length = patch_layer(layer_path, mode)
        patched_digest = file_sha256(layer_path, base, length)
        print(mode, "patched libc ELF sha256:", patched_digest, flush=True)
        new_diff_id = "sha256:" + file_sha256(layer_path)
        with open(layer_path, "rb") as raw, open(compressed_path, "wb") as compressed_file:
            with gzip.GzipFile(filename="", mode="wb", fileobj=compressed_file, compresslevel=1, mtime=0) as compressed:
                shutil.copyfileobj(raw, compressed, 8 << 20)
        new_layer_digest = "sha256:" + file_sha256(compressed_path)
        replacements = {
            blob_path(new_layer_digest): compressed_path,
        }
        config_paths = {}
        patched_manifests = 0

        def patch_descriptor(descriptor, archive):
            nonlocal patched_manifests
            item = json.load(archive.extractfile(blob_path(descriptor["digest"])))
            if "manifests" in item:
                changed = any([patch_descriptor(child, archive) for child in item["manifests"]])
                if not changed:
                    return False
            elif "layers" in item:
                if not item["layers"] or blob_path(item["layers"][0]["digest"]) != old_layer:
                    return False
                old_config = blob_path(item["config"]["digest"])
                config = json.load(archive.extractfile(old_config))
                config["rootfs"]["diff_ids"][0] = new_diff_id
                config_bytes = encoded(config)
                new_config = blob_path(sha256(config_bytes))
                replacements[new_config] = config_bytes
                config_paths[old_config] = new_config
                item["config"].update(digest=sha256(config_bytes), size=len(config_bytes))
                item["layers"][0].update(digest=new_layer_digest, size=compressed_path.stat().st_size)
                patched_manifests += 1
            else:
                raise AssertionError("unrecognized OCI descriptor")
            item_bytes = encoded(item)
            replacements[blob_path(sha256(item_bytes))] = item_bytes
            descriptor.update(digest=sha256(item_bytes), size=len(item_bytes))
            return True

        with tarfile.open(source) as archive:
            for descriptor in top["manifests"]:
                patch_descriptor(descriptor, archive)
        assert patched_manifests >= 2, "pinned bundle does not use shared libc layer"
        for entry in classic:
            if entry["Layers"][0] == old_layer:
                entry["Layers"][0] = blob_path(new_layer_digest)
                entry["Config"] = config_paths[entry["Config"]]
        replacements["index.json"] = encoded(top)
        replacements["manifest.json"] = encoded(classic)
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
    if len(sys.argv) not in (3, 4):
        raise SystemExit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3] if len(sys.argv) == 4 else "loop")
