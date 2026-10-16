# AMD leaf snapshot copy trial (2026-10-16)

This isolated hypervisor-only variant copies a guarded leaf page directly
into the VM's existing 4 KiB snapshot buffer. Root and forked VMs use their
stable guest-page pointers, avoiding eight 512-byte reads and a stack
staging buffer per protected page-table write. The default context method
retains the old chunked behavior for test implementations.

All 279 local `bedrock-vmx` tests and the complete in-box SVM suite passed on
disposable EPYC 4245P / HWE `7.0.0-38-generic` box `box-5e63aad0`. A
60-virtual-second diagnostic using pinned Tempo DST seed
`14373434789495652599` passed in **102 host seconds**. An adjacent unchanged
production control on the same box, guest assets, seed, and userspace driver
passed in **100 host seconds**. Both used progress driver SHA-256
`f0d4c4c16cd5a25549d83107b1b0161c4432769d0c42e26f2144c5994648f554`.
This short test does not establish a speed gain, so the variant is not merged
into production. The production module was restored and passed the SVM suite.

An earlier isolated 2 KiB stack-buffer trial passed the local and in-box SVM
suites but its box stopped responding during the 60-virtual-second campaign.
`boxctl` continued to report the box as ready while Tailscale showed it
offline. The box could not be inspected, so the candidate cannot be proven
to have caused the outage; it is rejected without a successful DST run. The
direct-buffer variant above avoids that larger stack allocation.

All module builds and loads occurred inside disposable boxes. No kernel
module was built or loaded on the workstation.
