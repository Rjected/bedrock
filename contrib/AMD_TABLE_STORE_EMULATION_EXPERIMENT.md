# Guarded page-table MOV emulation experiment

The safe AMD branch at `712cce1` uses an NPT write guard on guest page-table
pages. A common Linux PTE update (`MOV [base], r64`, encoding `89 /r`)
traps on that guard, then retires through a one-instruction TF replay. The
post-hold sample found this replay at `set_pte_range` and `unmap_page_range`.

This branch tests retiring only the simple, aligned, in-page, 64-bit MOV store
at the NPT exit. It checks the decoded base address against the fault GPA,
requires AMD's final-GPA translation marker (EXITINFO1 bit 32, documented in
the [AMD64 APM volume 2](https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content)), and falls back
to hardware replay for page-table-walk faults, pending events, debug state,
deadline boundaries, or unsupported encodings. The guard is invalidated before
the guest RAM store; root and forked memory writes avoid treating the retired
guest instruction as an unrelated host edit. The test exercises exact TSC
stopping and fork/COW writes.

Tests on `box-df9442c1` (EPYC 4245P, HWE `7.0.0-38-generic`) passed the full
SVM hardware suite and the pinned 180-virtual-second Tempo DST one-seed
campaign with `failed=0`. The same-box baseline finished in **192 host
seconds**. Experimental full runs measured **182** and **189 seconds** before
the final-translation guard, **192 seconds** with that guard and diagnostic
counting, and **191 seconds** with the guard and no counter. (The DST driver's
own last figure for that last run was 190 seconds.) The guarded build did
emulate over two million stores, but these results do **not** establish a
repeatable end-to-end speedup over the control.

This is an experimental branch, not a replacement for the safe production
branch. The unchanged NPT instruction-fetch and clock exits remain the larger
performance targets. No module was built or loaded on the workstation; all
hardware testing took place in the ephemeral `boxctl` box.
