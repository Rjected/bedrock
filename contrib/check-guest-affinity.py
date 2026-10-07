#!/usr/bin/env python3
"""Check guest CPU-count behavior with seconds-long KVM boots.

Usage: python3 contrib/check-guest-affinity.py /path/to/guest/bzImage
Requires gcc, cpio, gzip, QEMU and an available KVM backend.
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

if len(sys.argv) != 2:
    raise SystemExit(__doc__)
if Path("/sys/module/bedrock").exists():
    raise SystemExit("Bedrock owns virtualization; unload it before using KVM.")

kernel = Path(sys.argv[1]).resolve(strict=True)
source = Path(__file__).resolve().parent.parent / "guest/affinity-test.c"
with tempfile.TemporaryDirectory(prefix="bedrock-affinity-") as directory:
    work = Path(directory)
    root = work / "root"
    root.mkdir()
    for label, parameter, count in [
        ("default", "", 1),
        ("eight", "bedrock_ncpus=8", 8),
        ("zero", "bedrock_ncpus=0", 1),
    ]:
        subprocess.run(["gcc", "-O2", "-static", "-nostdlib", "-fno-stack-protector",
                        "-no-pie", f"-DEXPECT_NCPUS={count}", "-o", str(root / "init"),
                        str(source)], check=True)
        initrd = work / "initrd.gz"
        with initrd.open("wb") as output:
            subprocess.run(["bash", "-o", "pipefail", "-c",
                            "find . -print0 | cpio --null -o -H newc | gzip -1"],
                           cwd=root, stdout=output, stderr=subprocess.PIPE, check=True)
        command = ["qemu-system-x86_64", "-enable-kvm", "-cpu", "host", "-m", "256",
                   "-smp", "1", "-nographic", "-nodefaults", "-serial", "stdio",
                   "-monitor", "none", "-no-reboot", "-device",
                   "isa-debug-exit,iobase=0xf4,iosize=4", "-kernel", str(kernel),
                   "-initrd", str(initrd), "-append",
                   f"console=ttyS0 nokaslr mitigations=off panic=-1 {parameter}"]
        if not os.access("/dev/kvm", os.R_OK | os.W_OK):
            command = ["sudo", "-n", *command]
        start = time.monotonic()
        try:
            result = subprocess.run(command, capture_output=True, timeout=20)
        except subprocess.TimeoutExpired as error:
            print((error.stdout or b"")[-6000:].decode(errors="replace"))
            raise SystemExit(f"{label}: guest boot exceeded 20 seconds")
        if result.returncode != 33 or b"GUEST_AFFINITY_PASS" not in result.stdout:
            print((result.stdout + result.stderr)[-6000:].decode(errors="replace"))
            raise SystemExit(f"{label}: affinity check failed (exit {result.returncode})")
        print(f"{label}: affinity PASS ({time.monotonic() - start:.2f}s)", flush=True)
