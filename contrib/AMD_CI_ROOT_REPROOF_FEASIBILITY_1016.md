# AMD conditional-grant proof availability after CR3 changes (2026-10-16)

The pinned Tempo DST workload frequently revoked conditional NPT execute
grants at a CR3 change. A read-only diagnostic sampled one in 256 of those
root changes on disposable EPYC 4245P / HWE `7.0.0-38-generic` box
`box-2b77e4f7`. The same pinned seed `14373434789495652599` passed a
60-virtual-second campaign (`failed=0`) in 100 host seconds. The userspace
driver SHA-256 was
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.

At the last checkpoint, 147,456 root-change events yielded 576 samples.
The destination root had a ready, clean guarded tree in all 576 samples;
the other coarse code/NPT/host-write dependencies were unchanged in 575.
Cached single-page alias proofs for the destination root covered all
conditionally trusted pages in 495/576 samples (85.9%) and 957/1,078
individual page grants (88.8%). No sampled page had a boundary-continuation
cache entry matching the destination root and tree generation. The existing
cache has one direct-mapped entry per page and usually holds the old root's
continuation.

An alias proof alone is insufficient to retain NPT execute: the new root's
next virtual page must also translate to globally trusted code, and its
bytes must be checked for a cross-page hazardous instruction. The counts
are therefore an opportunity bound, not a correctness or speed result.
Raw counters and status are under ignored
`target/boxctl-evidence/box-2b77e4f7/root-proof/` in this worktree. All
module builds and loads happened inside the disposable box.
