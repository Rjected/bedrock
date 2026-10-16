# AMD CI entry-path profile, 2026-10-16

This branch adds temporary counters to identify general software costs in the
AMD SVM execution path. It is diagnostic instrumentation, not a production
optimization. All measurements below used the same pinned Tempo DST seed,
guest assets, and disposable `boxctl` host (`box-63bccca3`, HWE
`7.0.0-38-generic`). Each diagnostic campaign ran 60 virtual seconds and
passed with `failed=0`. Instrumentation changes absolute timings, so the
cycle totals are useful for attribution rather than an Intel comparison.

At about 42 million guest entries, entry preparation used about 164–169
billion TSC cycles. Plan selection was 86–89 billion cycles (about 53% of
preparation), guarded-tree refresh about 43 billion (26%), instruction-exit
preparation about 23 billion (14%), and protection about 3–4 billion.

Within plan selection, the global plan ran about 34 million times and used
about 74 billion cycles. Its breakpoint collector used about 53 billion
cycles, roughly 70% of the global planner. A separate collector outcome run
at 12.58 million calls counted 11.94 million cached proofs, 148 thousand
compositions, and 484 thousand successful fresh alias walks. Fresh walks were
only about 3.8% of calls but used about 42 billion collector cycles; cache
hits used about 0.7 billion. The walk count excludes unsuccessful early
returns.

The miss classifier saw about 391 thousand lookups finding a matching but
invalid proof, versus fewer than one thousand full cache sets. It also
counted roughly 24 thousand broad proof invalidations, mainly after upper
or inactive-root page-table writes, plus guard restores. In contrast, the
leaf-PTE diff path invalidated 983 proofs despite seeing over 675 thousand
new executable PTE entries. This rules out cache capacity and ordinary leaf
updates as the main source of repeated walks in this campaign. Broad
invalidation remains necessary when an upper link can expose a new alias;
any reduction needs an explicit proof about the affected roots or mappings.

These counters identify the next general optimization target: reduce the
cost of safe fresh alias enumeration or retain proofs more precisely across
upper-table changes. They do not yet show a production speedup. The
production pinned 180-virtual-second AMD run remains about 186–192 wall
seconds on this class of box, compared with the reported Intel run at about
65 seconds on different hardware and guest images.
