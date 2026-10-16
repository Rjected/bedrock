# Indexed alias-walk trial, 2026-10-16

The fresh alias walker linearly searches the guarded table list for each
upper-level table, although the same list has a hash index. This branch uses
that index and falls back to the previous linear search when it is unavailable.
It does not change the alias or breakpoint rules.

The 276 `bedrock-vmx` unit tests and the disposable `boxctl` SVM suite passed.
On `box-63bccca3`, with the same pinned guest assets and seed, the
60-virtual-second DST run passed in 105–106 wall seconds; adjacent production
passed in 111 seconds. The full 180-virtual-second run passed in 187–188 wall
seconds; adjacent production passed in 186–187 seconds. This did not produce
a reliable full-workload improvement, so the code was not promoted to the
production branch.
