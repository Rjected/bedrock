# AMD one-hazard reachable-region feasibility (2026-10-16)

The existing AMD global planner uses a reachable-code proof on certain
five-hazard pages. This read-only diagnostic asked whether extending it to
write-guarded, one-hazard pages could remove their only breakpoint.
Every 256th eligible entry copied the current plan, ran the region prover,
then restored its diagnostic cache insertion; guest execution still used
the unchanged page-wide plan. Two pinned 60-virtual-second Tempo DST runs
passed (`failed=0`) in 98 and 99 host seconds on disposable EPYC 4245P /
HWE `7.0.0-38-generic` box `box-2b77e4f7`. Their userspace driver SHA-256
was `f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.

At the first run's last checkpoint, 12,288 samples from 3,145,728 eligible
entries yielded 243 zero-breakpoint regions (2.0%), 7,132 one-breakpoint
regions, 3,303 regions with more breakpoints, and 1,610 failed proofs. The
second run's last checkpoint also had 12,288 samples, with 250 zero,
6,855 one, 3,553 many, and 1,630 failed. Its per-zero-region log recorded
269 zero cases by run end, led by physical pages `0x1634000` (123),
`0x11f6c6000` (61), and `0xbc91000` (49). None targeted the pinned guest's
hot scheduler page `0x1feb000`.

This is a sampled entry classifier, not a fault-count or performance gain.
The broad change would normally replace one exact breakpoint with one or
more proof-frontier stops; it would not remove the scheduler fault class.
No execution-path candidate was merged. Raw counters and statuses are under
ignored `target/boxctl-evidence/box-2b77e4f7/one-region/` in this worktree.
All module builds and loads occurred inside the disposable box.
