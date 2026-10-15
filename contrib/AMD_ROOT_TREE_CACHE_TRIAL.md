# AMD guarded root-tree cache trial (2026-10-15)

The post-execute-grant clock profile found that CR3-switch guarded-tree
rebuilds dominate measured clock-entry setup cycles. This isolated trial
stores two recently guarded root trees, reusing their table links only while
their guard epoch and NPT mapping generation remain unchanged. Guest table
guard release, host writes, dirty tree state, and remaps revoke reuse;
unproven roots fall back to the existing complete walk.

On disposable `box-a42f86b1`, all 276 VMX unit tests and the SVM hardware
suite passed. The exact pinned one-seed Tempo campaign passed in **110 host
seconds** with zero failures. The immediately following original-image,
production-module control also passed in **110 seconds**. The trial had
**148,566 snapshot hits in 400,000 full-tree probes** near the end of its
campaign, but no measured wall-time gain. It adds about two bounded tree
snapshots to each VM's guard workspace and remains isolated pending a
clear performance benefit and broader invalidation review. All module
builds and loads occurred only inside the box; it was returned to the
production module after the comparison.

Raw hit counts and campaign statuses are ignored under
`target/boxctl-evidence/box-a42f86b1/root-tree-cache/`.
