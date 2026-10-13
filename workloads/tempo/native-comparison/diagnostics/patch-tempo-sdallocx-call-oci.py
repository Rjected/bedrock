#!/usr/bin/env python3
"""Experimental exact-build redirect of one false REP-bearing Tempo call.

Usage: patch-tempo-sdallocx-call-oci.py ORIGINAL_IMAGES_TAR OUTPUT_IMAGES_TAR
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
PATCHED_TEMPO_SHA256 = "6672faef5559e3c8e82f8b3682ee47dc0d18aa92bed9405a8b100a89aa45c24e"
TEXT_BIAS = 0x1000
CALL_VA = 0x23755A0
STUB_VA = 0x2375575
CALL_BYTES = bytes.fromhex("ff 15 f2 a5 b4 05")
STUB_BYTES = bytes.fromhex("ff 25 1d a6 b4 05")


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


def patch_layer(layer_path):
    with tarfile.open(layer_path) as layer:
        member = layer.getmember("usr/local/bin/tempo")
        assert member.size == 1163218968, "unexpected Tempo ELF size"
        base = member.offset_data
    assert file_sha256(layer_path, base, member.size) == ORIGINAL_TEMPO_SHA256, "unexpected Tempo build"
    with open(layer_path, "r+b") as layer:
        layer.seek(base + CALL_VA - TEXT_BIAS)
        assert layer.read(6) == CALL_BYTES, "indirect call changed"
        layer.seek(base + STUB_VA - TEXT_BIAS)
        assert layer.read(6) == STUB_BYTES, "same-page jump stub changed"
        call_target = CALL_VA + 6 + struct.unpack("<i", CALL_BYTES[2:])[0]
        stub_target = STUB_VA + 6 + struct.unpack("<i", STUB_BYTES[2:])[0]
        assert call_target == stub_target, "GOT target changed"
        displacement = STUB_VA - (CALL_VA + 5)
        layer.seek(base + CALL_VA - TEXT_BIAS)
        layer.write(b"\xe8" + struct.pack("<i", displacement) + b"\x90")
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
