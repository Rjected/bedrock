# Current AMD hot-exit profile (2026-10-15)

The safe production module at `76cc6e9` was instrumented on disposable
`box-ad902887` (HWE `7.0.0-38-generic`) to count raw SVM exits and sample
one in 256 exit RIPs. The diagnostic changes only the module, not the
userspace driver or guest. It ran the pinned Tempo DST seed
`14373434789495652599` with guest `vmlinux` SHA-256
`3feabfd845b13320fc499b008747dac1c7f9991a87029879cb10ba0f940b2336`.
Both the 60-virtual-second diagnostic and the exact 180-virtual-second
campaign passed (`failed=0`). The latter took 190 host seconds **with the
diagnostic enabled**; this is not a speed comparison.

| Virtual-TSC collection window | Fetch NPT faults | Other NPT faults | Raw scalar `#DB` exits |
| --- | ---: | ---: | ---: |
| 100–250 billion, shorter campaign | 5,687,732 | 620,472 | 1,598,991 |
| 300–600 billion, pinned campaign | 13,988,906 | 665,955 | 2,204,423 |

Fetches were **95.5% of NPT faults** in the later window. Its leading sampled
fetch RIPs mapped to `__put_user_8`, `__schedule`,
`rcu_note_context_switch`, and `pick_task_rt`, plus several user-space code
sites. Leading data-fault RIPs mapped to `native_apic_mem_write`,
`handle_mm_fault`, `native_apic_mem_eoi`, and `unmap_page_range`. The top
scalar `#DB` RIP was `native_irq_return_iret`. The histogram had one fetch
collision among 54,645 samples and 91 scalar collisions among 8,612.

This confirms that repeated instruction-fetch permission changes on hot
hazardous pages, not page-table lookup or data-fault processing, dominate
current NPT traffic. The old, unsafe execute-grant trial had far fewer fetch
faults, but its grant outlived the proof guarding that code, so its 101-second
run cannot be used as a valid performance target. A general speedup must
retain executable pages only while their exact-byte hazard breakpoints,
code-write guards, and translation proofs remain active; revoke them before
an incompatible page plan, host write, or translation change. The current
four-breakpoint budget means any retained set must be bounded by its
combined hazards and executable aliases.

The raw histograms and diagnostic source are retained under
`target/boxctl-evidence/box-ad902887/current-exit-profile/` in this
workspace. All module builds and loads occurred in the box.
