# AMD multi-page clock hold experiment

On EPYC 4245P box `box-266a001a` with HWE `7.0.0-38-generic`, this
isolated candidate retains the token-backed NPT execute grants for all
selected code pages across consecutive clock exits. The existing single-page
hold retained only the first grant; other selected pages were revoked after
each entry. Every new entry still rearms its code-write guard, and retained
grants are revoked if the guest event, code epoch, NPT mapping generation,
CR3, host writes, selected page list, or RUN lifetime changes.

The first version passed 276 VMX library tests and the box SVM suite but
stopped making progress near virtual second 217 in the exact pinned Tempo
DST campaign. It was terminated after 337 host seconds with no workload
verdict. A diagnostic two-page clock probe showed that the candidate selected
multi-page holds but did not reuse its guarded clock plan: a scalar-page hint
from an earlier fetch fault blocked reuse even though that page was in the
selected batch. The revised candidate treats that hint as satisfied only
when it names a page in the still-validated selected batch. It also avoids
setting the scalar-page hint after a two-page clock exit. A box-only
diagnostic confirmed repeated multi-page plan reuse; the diagnostic logging
was removed before the full campaigns.

The revised candidate passed all 276 VMX library tests and the full in-box
SVM suite. A new default SVM hardware probe alternates between two hazardous
code pages while reading the clock, crosses an exact instruction deadline,
then writes an RDRAND instruction into one page and verifies interception.
It passed with five NPT faults. All kernel-module builds and loads occurred
only in the disposable box.

The exact pinned one-seed, 180-virtual-second Tempo DST campaign passed with
`failed=0` in **194, 197, and 197 host seconds** on the revised candidate.
Unchanged production controls on the same box passed in **201, 202, and 207
host seconds**. The guest kernel, initrd, images, seed, and userspace driver
SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`
were held fixed. These runs suggest a small gain, with the sample too small
to establish its precise size. The reported roughly 65-second Intel CI run
used a different host and guest kernel; this change does not close that gap.
