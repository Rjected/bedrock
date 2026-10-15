# AMD hot exits after retained NPT execute grants (2026-10-15)

This diagnostic rebases the bounded RIP histogram on the pushed
`dan/amd-tempo-dst` execute-grant change. It samples one in 256 raw SVM
exits between virtual TSC 150 and 600 billion and adds hot-path atomics;
its elapsed time is not a production comparison. On disposable
`box-a42f86b1`, the SVM suite and exact pinned one-seed Tempo campaign
passed, the latter in 111 host seconds.

At the dump, raw instruction-fetch NPT faults were **579,956**, other NPT
faults **1,270,091**, and scalar #DB exits **3,318,227**. The earlier
pre-grant histogram observed 21,571,998 fetch faults in the same virtual
TSC window. Hot post-grant data-fault RIPs include
`native_apic_mem_write`/`native_apic_mem_eoi`, `handle_mm_fault`,
`set_pte_range`, and `unmap_page_range`. The leading sampled #DB site is
`native_irq_return_iret`, followed by those memory-management paths and
user instruction-page boundaries. Fetch faults are now dispersed among
user-space sites rather than the earlier kernel scheduling pages.

The raw histogram is ignored under
`target/boxctl-evidence/box-a42f86b1/hot-after-hold/histogram.txt` in this
worktree. The host kernel module was built and loaded only inside the box.
