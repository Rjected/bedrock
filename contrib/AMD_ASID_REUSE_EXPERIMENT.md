# Guarded clock-exit ASID reuse experiment

On EPYC 4245P box `box-266a001a` with HWE `7.0.0-38-generic`, an
experimental SVM change kept the previous ASID only when the same guarded
single-page NPT execute grant and clock batch were reused on the next entry.
All other entries still receive a fresh per-CPU ASID. The candidate passed
276 `bedrock-vmx` library tests and the complete in-box SVM suite, including
fork replay, guarded code writes, and exact instruction deadlines. Only the
disposable box built or loaded a kernel module.

The pinned one-seed Tempo DST campaign passed with `failed=0` in 195 and 199
host seconds on the candidate. An unchanged production module on the same box
passed the same campaign in 207 host seconds between those candidate runs.
An additional unchanged control after both candidate runs passed in 201
host seconds. These alternating results narrow the apparent advantage to a
few seconds, which may be ordinary run variation.
The patched guest kernel, initrd, images, seed, 180-virtual-second interval,
and userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`
were the same. The result needs more repeated controls before claiming a
stable effect. It does not close the gap to the
reported approximately 65-second Intel CI run, which used a different host
and guest kernel.

The reuse condition depends on retaining the `NptExecuteGuard` token across
consecutive clock exits and checking the guarded page's code epoch, NPT
mapping generation, guest CR3, host writes, pending event, and reschedule
state before the next VMRUN. If any check fails, the grant is restored and a
fresh ASID is allocated. This is an isolated experiment; the production AMD
branch remains at its corrected execute-grant lifetime baseline.
