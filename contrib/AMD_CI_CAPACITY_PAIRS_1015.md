# AMD CI breakpoint-capacity pair profile (2026-10-15)

This diagnostic branch samples category-6 instruction-fetch NPT faults during
the pinned one-seed, 180-virtual-second Tempo CI workload. The logs come from
the disposable `box-e675c0c1` with HWE `7.0.0-38-generic`; the workstation
never built or loaded a kernel module. Both diagnostic campaigns passed the
workload (`failed=0`) in 183 and 192 host seconds.

Of 755 sampled capacity faults in the first run, 114 were transitions from
GPA `0x1398a2000` to `0xfd49000`, and 71 were the reverse. This unordered pair
therefore accounted for 185/755 (24.5%) of sampled capacity faults. The next
directed transitions were `0xfd49000` to `0xfd37000` (57) and `0xfd37000` to
`0x1398a2000` (44). Sampling is periodic, so these counts describe the samples
and are not an exact total-fault census.

The second run recorded target RIP offsets and one-time page fingerprints.
Matching the exact FNV-1a page hashes against the pinned CI initrd identified:

| GPA | Initrd file page | Raw hazard offsets | Common target offset |
| --- | --- | --- | --- |
| `0x1398a2000` | `libsystemd-shared-259.so` at `0xb6000` | `0x172, 0x372, 0x572, 0x772` | `0x310` from libc |
| `0xfd49000` | `libc.so.6` at `0xc2000` | `0xffd, 0xffe, 0xfff` | `0xb30`, `0xef0`, `0xc10` from systemd |
| `0xfd37000` | `libc.so.6` at `0xb0000` | `0xffe, 0xfff` | `0x850` from libc |

The four systemd hazards are false `F2` matches inside PLT jump displacements.
The adjacent same-box experiment used the production module and driver, and
changed only the guest initrd using the existing SHA-checked
`guest/patches/relocate-systemd-plt-displacements.py` patch. The patch is a
diagnostic upper-bound experiment; it is not a general hypervisor fix.

| CI guest | Initrd SHA-256 | Result | Host wall time |
| --- | --- | --- | --- |
| Relocated systemd PLT | `53ca068663e3ce162196d8b5025868095579196fcb5c1a82e89bdc9b3104e9b4` | pass, `failed=0` | 183 s |
| Original pinned CI | `f12e0c29c227b93ab9e05cd8791fb00a0508ce57c2d0b5e9d719ec396914e43f` | pass, `failed=0` | 188 s |

The five-second difference is within observed run-to-run variation (the two
unmodified diagnostic runs themselves differed by nine seconds). It does not
establish a repeatable speedup. Production `dan/amd-tempo-dst` remains unchanged
and pushed at `321c642`. On the pinned one-seed workload AMD remains roughly
189–193 host seconds against approximately 65 seconds for the analogous Intel
run on different hardware; this is an indicative workload comparison, not a
same-host CPU comparison. The remaining work is a safe, general hypervisor
mechanism that avoids capacity fetch faults across arbitrary guest pages and
demonstrates repeatable CI improvement.

Raw kernel logs and campaign status files are under ignored
`target/boxctl-evidence/box-e675c0c1/ci-capacity-pairs/` in this worktree.
