# Retained page-table guard index trial (2026-10-15)

After the production NPT execute-grant change, an IBS-paused host profile on
disposable `box-a42f86b1` attributed 8.33% of all CPU1 samples to
`indexed_retained_guard_for`. That function falls back to a linear registry
scan after the first new guard is installed during a guarded-tree rebuild.
This isolated trial indexes every live guard, keeps duplicate entries, and
inserts reused slots under their new page hash. A lookup chooses the lowest
live registry index, preserving the old linear scan's result. The 8,192-slot
index has room for 4,096 retained guards plus at most 1,024 insertions in a
single rebuild. Collision, duplicate, slot-reuse, and repeated differential
lookup tests passed; all **276 VMX tests** and the in-box SVM hardware suite
passed.

The exact pinned one-seed Tempo campaign passed with zero workload failures
in **115 host seconds**. A separate IBS-paused diagnostic campaign also
passed in **115 seconds** versus **116 seconds** for the unchanged
IBS-paused diagnostic control. In that second profile the indexed lookup no
longer appeared among symbols above 0.5% of all CPU1 samples, but unrelated
host work and the diagnostic MSR operations affect sample shares. The
production branch's recent unprofiled full runs were about **110–116
seconds**, so there is **no demonstrated wall-time improvement**. This
candidate is isolated and not merged.

The paired raw IBS recordings and statuses are ignored under
`target/boxctl-evidence/box-a42f86b1/ibs-after-hold/` in the production
checkout. All kernel-module builds and loads took place only inside the box.
