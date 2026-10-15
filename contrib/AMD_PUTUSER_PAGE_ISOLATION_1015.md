# AMD `put_user` page-isolation trial (2026-10-15)

The current raw-exit profile places `__put_user_8` among the hot instruction-fetch
NPT faults. In the Clang 18 control guest, `kaslr_get_random_long` starts at
`0xffffffff81fe3710` and `__put_user_8` at `0xffffffff81fe39c0`: both are on
the same 4 KiB executable page. The former contains RDRAND, a hazard that
requires guarding the page. An experimental guest patch aligns the
`arch/x86/lib/putuser.S` object at both ends; the rebuilt guest places
`__put_user_8` at `0xffffffff81fe4100` and leaves `kaslr_get_random_long` at
`0xffffffff81fe3710`, on separate pages. The patch is isolated on
`dan/amd-putuser-page-isolation-1015` and is not part of the production branch.

On disposable HWE box `box-ad902887`, the same pinned Tempo CI workload,
seed `14373434789495652599`, initrd, images, userspace driver, and Clang 18
kernel config gave the following full 180-virtual-second campaigns:

| Guest | Failed | Host wall time |
| --- | ---: | ---: |
| Clang 18 control, concurrent with candidate kernel build | 0 | 231 s |
| Isolated `putuser.S` page | 0 | 220 s |
| Clang 18 control, no concurrent build | 0 | 226 s |

The clean paired difference is six seconds (2.7%) from one seed and is not
enough to establish a repeatable improvement. Both builds are also slower
than the existing Nix-built guest's ~190–193 s on this box; changing the
toolchain and code layout changes the baseline, so 220 s must not be compared
directly with 190 s to assess this patch. The first control ran concurrently
with a kernel build and is excluded from the paired comparison.

This is guest layout shaping, not a general AMD-V execution acceleration.
Even if repeated runs establish the small benefit, it cannot close the gap
to the analogous Intel run (~65 s on a different host and guest). The main
bottleneck remains cross-page NPT fetch churn under the four-breakpoint
proof limit. Raw campaign statuses and logs are retained locally under
`target/boxctl-evidence/box-ad902887/putuser-page-isolation/`. Kernel builds
and all module operations occurred only inside the disposable box.
