# AMD clock-exit entry phases after execute-grant retention (2026-10-15)

This isolated branch adds TSC reads and atomics around SVM entry stages.
Its campaign elapsed times are diagnostic, not production comparisons. Four
exact pinned Tempo campaigns passed on disposable `box-a42f86b1`; all 275
VMX unit tests and the in-box SVM hardware suite passed. Only the box built
or loaded kernel modules.

At 16 million clock exits in the final campaign, setup accounted for
**25.05 billion** measured TSC cycles, instruction preparation **6.56
billion**, plan selection **7.74 billion**, and guard setup **0.90 billion**.
The tree-refresh call accounted for **22.82 billion** of setup cycles.

Of those 16 million refresh calls, **15.74 million** met the guarded-tree
fast-return conditions and used **0.74 billion** cycles. The remaining
~257,000 calls used **22.07 billion** cycles. **246,809** clock entries
followed a guest CR3 switch, **9,905** saw a dirty gate, and **346** saw a
retained-guard mapping-generation mismatch; these categories can overlap.
No sampled clock entry lacked ROGPT. The existing retained one-page execute
grant was not active at the start of these clock entries, so its refresh
skip does not address this cost. The next general target is reuse of an
unchanged guarded translation tree across CR3 switches, with invalidation
on table writes, host writes, and NPT remaps.

The raw counters and campaign statuses are ignored under
`target/boxctl-evidence/box-a42f86b1/entry-phase-after-hold/`.
