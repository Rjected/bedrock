# AMD debug-exit counter diagnostic

This isolated branch instruments counted SVM debug exits; it is not a
production change. It is based on the guest PMC clock trial plus the shared
IRPERF debug-exit correction. The diagnostic logs a one-tick disagreement
when programmable PMC0 exceeds VMCB IRPERF and records whether IRPERF
matches a decoded bounded endpoint.

On disposable `box-0598ea76` (EPYC 4245P, HWE `7.0.0-38-generic`), the
box SVM suite and a full one-seed Tempo campaign with the PMC clock enabled
passed. The campaign took **102 host seconds** while a 40-second host perf
sample ran during its seed. The kernel journal logged **8** debug-exit
disagreements. Several immediate breakpoints had VMCB IRPERF **1** and
programmable PMC0 **2**, with RIP unchanged; the endpoint-only correction
would miss them. None of the eight had IRPERF matching a decoded endpoint.
The logs are preserved under ignored
`target/boxctl-evidence/box-0598ea76/debug-tick/`.

The original failed campaign on `box-b47a5eea` reached `_copy_to_user` with
VMCB IRPERF **6**, PMC0 **7**, and a decoded five-instruction bounded batch.
The common AMD fix skips programmable-counter promotion on debug exits and
retains it on ordinary exits. This diagnostic supports that scope, but does
not itself establish how often the original `_copy_to_user` case occurs.
Only disposable boxes built or loaded kernel modules.
