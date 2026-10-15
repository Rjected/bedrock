# AMD exact-state global-plan cache trial (2026-10-15)

An isolated hypervisor-only candidate cached the most recent counted global
SVM plan per vCPU. It reused a plan only at the same entry RIP, physical page,
CR3, recent-page set, and predicted successor, with unchanged guarded-tree,
code, NPT mapping, and trusted-code generations. It additionally required
each untrusted admitted page to retain a guarded hazard memo and a live
single-page alias proof. The existing protection and execute-grant path still
ran on every entry. These conditions kept previously tested successor changes
and alias-proof revocations from reusing a stale plan.

The candidate on `dan/amd-ci-global-plan-cache-1015` passed all 276 VMX
library tests and the in-box SVM suite. On disposable HWE box `box-02f5f816`,
the same pinned one-seed, 180-virtual-second Tempo DST campaign passed in each
run with `failed=0`:

| Run order | Variant | Host wall time |
| --- | --- | ---: |
| 1 | Exact-state cache candidate | 185 s |
| 2 | Unchanged production control | 191 s |
| 3 | Same cache candidate | 193 s |

The apparent six-second gain reversed on repeat. The cache is **not merged**;
these runs do not establish a repeatable performance improvement. Exact-RIP
matching likely limits reuse, but this trial did not measure hit rate and
does not justify relaxing the key without additional entry-offset and
reachable-region safety checks.

The production module was restored inside the box. One restoration suite run
had a one-retirement `svm_bench` deadline assertion failure at
`crates/bedrock-vm/examples/svm_bench.rs:1870`; an immediate clean rebuild and
rerun of the unchanged production module passed the complete SVM suite. This
intermittent failure was not observed in the candidate suite or any of the
three full DST campaigns. Raw campaign logs and status files are under ignored
`target/boxctl-evidence/box-02f5f816/ci-global-plan-cache/`. No kernel module
was built or loaded on the workstation.
