# Retain AMD alias proofs across checked upper-table writes

An active upper-level guest page-table write previously cleared every cached
executable-alias proof as soon as Bedrock released the write guard. The global
gate already blocks proof use until that store retires, and its rebuild has
an existing check that compares the released upper table with the old
executable links. This change delays invalidation until that check. It keeps
only proofs the check accepts and clears them if the tree scan fails. Writes
to inactive-root tables still clear all proofs.

The 277 `bedrock-vmx` unit tests passed, including a new regression test for
an unchanged upper link and a changed executable link. The disposable
`boxctl` SVM suite passed. On `box-63bccca3` with identical pinned Tempo
DST guest assets and seed, two 180-virtual-second candidate runs passed in
185 and 182 wall seconds; adjacent production controls passed in 186 and
193 seconds. A 60-virtual-second diagnostic run passed in 108 seconds.

The improvement is modest and these timings include host variation. The
same pinned run was previously about 186–192 seconds on production AMD;
the reported Intel run of about 65 seconds used different hardware and
guest images. This change does not close that gap. The remaining large
collector cost is fresh alias enumeration after proof invalidation,
including writes to inactive-root tables.
