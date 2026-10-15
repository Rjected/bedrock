# AMD hot exit diagnostic (2026-10-15)

This isolated branch samples one in 256 raw SVM exits at virtual TSC
150–600 billion and logs a bounded RIP histogram. It adds atomic work on
VM exit and is **not** a timing candidate. Build and load its module only in
a disposable `boxctl` box, never on the workstation.

On `box-a42f86b1` with HWE `7.0.0-38-generic`, 275 VMX tests and the box SVM
suite passed. Two exact pinned one-seed Tempo campaigns passed in 204 and
200 host seconds. The first histogram printed nothing because `log_info!`
compiles out without `KERNEL_LOG=1`; the second used `kernel::pr_info!` and
printed the expected counts. Those times include diagnostic overhead and
must not be compared to the production or Intel runs.

At the dump point, the module observed 21,571,998 instruction-fetch NPT
faults and 993,667 other NPT faults: **95.6% of NPT faults were fetches**.
It sampled 84,266 fetch faults with only 11 hash collisions. The top kernel
fetch RIPs were `__put_user_8` (5,545 samples), two sites in `__schedule`
(5,163 and 3,731), `rcu_note_context_switch` (4,894), and `pick_task_rt`
(4,610). Several user-space sites were similarly hot. The top 12 sites
accounted for about 47% of sampled fetch faults.

The module observed 4,034,796 raw scalar `#DB` exits, sampled 15,761 with
411 hash collisions. The top RIPs included one user page-end site (1,940
samples), `native_irq_return_iret` (1,467), `handle_mm_fault` (720),
`unmap_page_range` (441), and several other user page-end sites. The
unclassified raw `#DB` count is a diagnostic rather than the runner's
synthetic-step count; its sample collision rate was 2.6%.

An exact-byte check of the pinned guest kernel explains three hot fetch
pages. The `__put_user_8` page also contains an actual `RDRAND` in
`kaslr_get_random_long`. The `__schedule` page has a false `f2 ad` hazard
inside the immediate address of a `MOV` in `is_mmconf_reserved`. The
`rcu_note_context_switch` and `pick_task_rt` pages end in partial `0f`
branch instructions, conservatively guarded as possible cross-page hazards.
This points to repeated execute-permission rearming and planning on pages
whose hot entry paths are safe, rather than just slow NPT fault handling.
Any persistent executable-page optimization must revoke its proof on code
writes, translation changes, and changes to a cross-page continuation.

The full kernel histogram is retained in ignored
`target/boxctl-evidence/box-a42f86b1/hot-exit/histogram.txt`. The box was
restored to the production module and its full SVM suite passed afterward.
