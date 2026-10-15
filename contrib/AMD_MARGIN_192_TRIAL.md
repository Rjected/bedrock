# AMD deadline margin 192 trial (2026-10-15)

The scalar-exit diagnostic found about 1.60 million exits near the
256-instruction safety margin of the AMD asynchronous PMU counter. This
isolated trial reduced that margin to 192 without changing the deadline
algorithm. It is **not** a production safety proof: the original 256 margin
exceeds a previously measured 135-retired-instruction overflow lag, and a
smaller value leaves less room for tail latency on other hosts or workloads.

All 275 VMX unit tests and the disposable-box SVM suite passed, including
exact deadline and code-write cases. With the same original guest image,
pinned Tempo seed, and userspace driver on `box-a42f86b1`, the exact campaign
passed in **120 host seconds** with margin 192. After restoring the production
256-margin module, the immediately following exact control passed in
**116 seconds**. The smaller margin showed no wall-time improvement and is
left isolated. Module builds and loads occurred only in the box.
