# AMD guarded boundary-code trust (2026-10-16)

The pinned Tempo CI profile found that roughly 55% of scalar instruction-fetch
NPT faults targeted pages with exactly one hazardous byte offset. In a
one-in-8,192 sample of scalar exits, 494 of 1,067 one-hazard samples had
their only offset at `0xfff`. The existing guarded single-alias continuation
proof succeeded on 1,776,708 of its first 3,145,728 boundary candidates in
a 60-virtual-second diagnostic, but each later cross-page entry still
fetch-faulted after the temporary execute grant was restored.

This hypervisor-only change retains NPT execute permission for a page after
that existing proof succeeds. It does so only when the page has a guarded
exact-byte memo, exactly one executable alias in the current guarded CR3,
and a non-hazardous continuation into a globally trusted next page. The
conditional grant is revoked before guest re-entry on a CR3 change, dirty or
replaced translation tree, code-epoch change, NPT mapping change, host write,
or trusted-code generation change. A recurrent-code guard is never released
for a writable retry until its conditional execute grant is revoked. A
software NPT bit marks these root-dependent grants so a forked VM starts the
corresponding page non-executable instead of inheriting the parent's proof.

All 19 `bedrock-ept` and 280 `bedrock-vmx` library tests passed, including
focused fork-copy, boundary revocation, and guarded-write ordering tests.
The complete in-box SVM suite passed on disposable EPYC 4245P / HWE
`7.0.0-38-generic` box `box-5e63aad0`. The exact pinned 180-virtual-second
Tempo DST seed `14373434789495652599` passed twice with the candidate and
once with adjacent unchanged production, using the same guest assets and
userspace driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`:

| Module | Verdict | Host wall time |
| --- | --- | ---: |
| Candidate, first revision | `failed=0` | 165 s |
| Unchanged production `5fc001a` | `failed=0` | 183 s |
| Candidate, final pre-entry revocation ordering | `failed=0` | 173 s |
| Pushed production code `28a2693`, rebuilt from production tree | `failed=0` | 171 s |

The first candidate branch-VM snapshot ended with 43.58 million exits,
including 22.73 million NPT faults; the control had 47.68 million exits and
26.32 million NPT faults. Their virtual snapshot endpoints differed by about
4.5 seconds, so these totals are directional rather than an exact aligned
exit-rate comparison. The same-box wall timing and successful repeat support
an approximately 10–18-second gain on this seed, not an Intel-equivalent
result. A further two-seed, full-duration campaign on the final code passed
both `14373434789495652599` and `14373434789495652600` (`failed=0`) in 304
host seconds total.
The pushed production branch also passed the complete in-box SVM suite before
its final 171-second pinned campaign.

Raw statuses are under ignored
`target/boxctl-evidence/box-5e63aad0/conditional-boundary/` in this
worktree. All kernel module builds and loads occurred inside disposable
`boxctl` boxes; none occurred on the workstation.
