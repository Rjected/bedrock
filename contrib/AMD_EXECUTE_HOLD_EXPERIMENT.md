# AMD single-page execute grant across clock exits

The SVM global gate temporarily grants NPT execute to a proven code page and
reverses that grant after every VM exit. The exact Tempo DST workload generated
roughly 21.6 million instruction-fetch NPT exits at the diagnostic dump point.
An isolated plan-cache-only trial reused a previously proved global plan after
RDTSC/RDTSCP but still reversed NPT execute on each exit. It passed the exact
one-seed campaign in 199 host seconds, indistinguishable from the production
200–202-second range on the same box.

This branch additionally keeps the NPT execute grant across consecutive
RDTSC/RDTSCP exits **on one global-gate page**. It restores the per-entry code
write guard after each exit and rearms it before each guest entry. It revokes
the retained execute grant before a host write, mapping or code revision,
translation-root change, pending guest event, failed plan validation, scalar
replay, or return from RUN. Reuse checks the current physical instruction page,
CR3, flags, guarded tree, instruction budget, and code/mapping generations.
Multi-page grants still use their original per-entry lifetime. The retained
grant is a local RUN-loop token and cannot survive into userspace.

All 275 `bedrock-vmx` unit tests and the in-box SVM hardware suite passed.
The added `clock-guard-write` hardware case ran 64 clock-loop iterations,
crossed an exact deadline, modified the same guarded code page, and then
observed the patched RDRAND intercept. It recorded only four NPT exits; the
test now fails if the count reaches 20. Exact pinned one-seed Tempo campaigns
on `box-a42f86b1` (EPYC 4245P,
HWE `7.0.0-38-generic`, original guest image) passed with zero workload
failures in **117, 115, 110, and 112 host seconds**. The final run includes the
host-write cache invalidation fix and returned **112 seconds**. A same-box,
unchanged production control passed in **195 host seconds**. The first seed had about **2.1 million
NPT exits** near virtual second 229, versus about **26.9 million** near virtual
second 224 in the plan-cache-only run. All campaigns used the pinned userspace
driver SHA-256 `08715a38504521806f70f2bcb518b501353db80c21f697b9138adc75930befb2`.
These timings do not establish an Intel-equivalent result: the reported Intel
CI run took about 65 seconds on a different host and guest kernel.

No kernel module was built or loaded on the workstation. All module builds,
loads, and hardware tests ran only inside the disposable box.
