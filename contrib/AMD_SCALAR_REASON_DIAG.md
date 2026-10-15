# AMD scalar-exit reasons after execute-grant retention (2026-10-15)

This isolated diagnostic labels scalar SVM `#DB` exits and the first global
page-planner rejection. It adds atomic work to the exit path and is not a
performance candidate. The exact pinned Tempo campaign passed three times
on disposable `box-a42f86b1`; all 275 VMX unit tests and the box SVM suite
also passed. All kernel module builds and loads occurred only in that box.

At the virtual TSC 600-billion dump in the final campaign, the diagnostic
counted **1,802,656** scalar exits with no safe batch, **800,783** forced
one-instruction replays, **357,942** guarded-write replays, **23,529**
entries remaining on an untrusted scalar page, and **248** on a trusted
scalar page. The untrusted-page count was about 300,000 in each of the first
two campaigns, so category sizes vary across runs. The sampling window and
exit instrumentation make these counts diagnostic,
not a wall-time breakdown.

Of the 1,802,656 no-batch exits, **1,599,295** occurred when the remaining
instruction budget was at or below the AMD counter's 256-instruction safety
margin. Another **203,229** had guest TF or RF set; only **132** started at
REP. The measured no-batch group had zero rejections from a dirty translation
gate, a missing hazard memo, an unguarded page boundary, or exhausted page
breakpoint slots. This makes the deadline tail the next general algorithmic
target. Reducing the margin alone would weaken the guarantee against late
asynchronous PMU overflow; the existing 256-instruction value exceeds a
previously observed 135-instruction lag. A safe speedup needs a more exact
way to run the tail, or evidence that a smaller margin is sound across the
supported hosts and workloads.

The forced replay histogram concentrated on `native_irq_return_iret` and
user page-end instructions. The no-batch RIPs were distributed across user
code rather than one hot loop. Raw histograms and campaign status files are
ignored under `target/boxctl-evidence/box-a42f86b1/scalar-reason/`.
