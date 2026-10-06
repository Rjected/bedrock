#!/usr/bin/env python3
"""Run a Bedrock executable in a diskless Linux host with nested SVM.

Requires qemu-system-x86_64, /dev/kvm, busybox-static, cpio, and gzip.
Kernel/module are built by `nix build .#kernel .#bedrockModuleDebug`.
The executable can be a Cargo hardware test or bedrock-cli. Files named by
--file are copied into the test host at /<basename>. Logs survive host reboots.
"""
import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--kernel", required=True, type=Path)
p.add_argument("--module", required=True, type=Path)
p.add_argument("--binary", required=True, type=Path)
p.add_argument("--expect", required=True, help="Required guest success marker in serial output")
p.add_argument("--file", action="append", default=[], type=Path)
p.add_argument("--timeout", type=int, default=120)
p.add_argument("--output", type=Path, default=Path("target/svm-evidence"))
p.add_argument("args", nargs=argparse.REMAINDER)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
root = a.output / "root"
if root.exists():
    shutil.rmtree(root)
for d in ["bin", "dev", "proc", "sys", "tmp"]:
    (root / d).mkdir(parents=True, exist_ok=True)

def copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755)

copy("/bin/busybox", root / "bin/busybox")
for app in ["sh", "mount", "insmod", "mkdir", "mknod", "poweroff", "dmesg",
            "cat", "sleep", "ls", "timeout", "tail"]:
    (root / "bin" / app).symlink_to("/bin/busybox")
copy(a.binary, root / "test")
# Preserve each ELF loader/library's absolute path in the diskless host.
ldd = subprocess.run(["ldd", str(a.binary)], capture_output=True, text=True)
for line in ldd.stdout.splitlines():
    for word in line.split():
        if word.startswith("/") and Path(word).is_file():
            copy(word, root / word.lstrip("/"))
copy(a.module, root / "bedrock.ko")
for source in a.file:
    copy(source, root / source.name)
args = a.args[1:] if a.args[:1] == ["--"] else a.args
command = shlex.join(["/test", *args])
(root / "init").write_text(f"""#!/bin/sh
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
if insmod /bedrock.ko; then
    timeout {a.timeout} {command}
    result=$?
else
    result=125
fi
echo BEDROCK_TEST_EXIT=$result
dmesg | tail -50
poweroff -f
""")
(root / "init").chmod(0o755)
initrd = (a.output / "host-initrd.gz").resolve()
with initrd.open("wb") as output:
    subprocess.run(["bash", "-o", "pipefail", "-c",
                    "find . -print0 | cpio --null -o -H newc | gzip -1"],
                   cwd=root, stdout=output, check=True)
qemu = ["qemu-system-x86_64", "-enable-kvm", "-cpu", "host", "-smp", "2",
        "-m", "4096", "-kernel", str(a.kernel.resolve()), "-initrd", str(initrd),
        "-append", "console=ttyS0 rdinit=/init panic=1", "-nographic", "-no-reboot"]
if not os.access("/dev/kvm", os.R_OK | os.W_OK):
    qemu = ["sudo", "-n", *qemu]
log = a.output / "serial.log"
with log.open("wb") as output:
    try:
        subprocess.run(qemu, stdout=output, stderr=subprocess.STDOUT,
                       timeout=a.timeout + 30, check=True)
    except (subprocess.TimeoutExpired, subprocess.CalledProcessError) as e:
        print(f"Test host failed: {e}; inspect {log}")
        raise SystemExit(1)
text = log.read_text(errors="replace")
print(log.resolve())
if "BEDROCK_TEST_EXIT=0" not in text or a.expect not in text:
    print(text[-6000:])
    raise SystemExit(1)
