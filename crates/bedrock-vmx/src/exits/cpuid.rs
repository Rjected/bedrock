// SPDX-License-Identifier: GPL-2.0

//! CPUID exit handler.

use super::helpers::{advance_rip, ExitHandlerResult};

#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

/// Handle CPUID exit: execute on the host and filter the results.
pub fn handle_cpuid<C: VmContext>(ctx: &mut C) -> ExitHandlerResult {
    let gprs = ctx.state().gprs;
    let leaf = gprs.rax as u32;
    let subleaf = gprs.rcx as u32;

    let (mut eax, mut ebx, mut ecx, mut edx) = cpuid(leaf, subleaf);

    match leaf {
        0x0 => {
            // Vendor string passes through; cap the max basic leaf.
            if eax > 0x16 {
                eax = 0x16; // Cap at highest leaf we handle
            }
        }
        0x1 => {
            // Fixed signature: Family 6, Model 85 (Skylake-SP), Stepping 7
            // (ext_model=5, model=5, family=6, stepping=7).
            eax = 0x00050657;

            // Clear VMX (ECX bit 5).
            ecx &= !(1 << 5);

            // Clear TSC_DEADLINE_TIMER (ECX bit 24); guest falls back to the
            // one-shot APIC timer (like bhyve).
            ecx &= !(1 << 24);

            // Set hypervisor present (ECX bit 31).
            ecx |= 1 << 31;

            // XSAVE/OSXSAVE/AVX and APIC pass through from the host.

            // Clear HTT (EDX bit 28): single logical processor.
            edx &= !(1 << 28);

            // EBX[31:24] APIC ID = 0, EBX[23:16] logical processor count = 1.
            ebx = (ebx & 0x0000FFFF) | 0x00010000;
        }
        0x4 => {
            if subleaf == 0 {
                // Report single core
                eax &= !0x3FFC000;
            }
        }
        0x6 => {
            // Thermal/power management: report none (host thermal state is
            // non-deterministic).
            eax = 0;
            ebx = 0;
            ecx = 0;
            edx = 0;
        }
        0x7 => {
            if subleaf == 0 {
                // EBX: pass through fsgsbase(0), bmi1(3), avx2(5), bmi2(8),
                // erms(9); clear unsupported features.
                ebx &= !(1 << 2); // SGX - not supported
                ebx &= !(1 << 4); // HLE (TSX) - not supported
                ebx &= !(1 << 7); // SMEP - requires CR4.SMEP handling
                                  // Bit 10 (INVPCID) passed through - enabled via ENABLE_INVPCID control
                ebx &= !(1 << 11); // RTM (TSX) - not supported
                ebx &= !(1 << 16); // AVX512F - not supported (requires extended XSAVE)
                ebx &= !(1 << 17); // AVX512DQ - not supported
                ebx &= !(1 << 18); // RDSEED - not supported
                ebx &= !(1 << 19); // ADX - not supported
                ebx &= !(1 << 20); // SMAP - requires CR4.SMAP handling
                ebx &= !(1 << 21); // AVX512_IFMA - not supported
                ebx &= !(1 << 26); // AVX512PF - not supported
                ebx &= !(1 << 27); // AVX512ER - not supported
                ebx &= !(1 << 28); // AVX512CD - not supported
                ebx &= !(1 << 30); // AVX512BW - not supported
                ebx &= !(1 << 31); // AVX512VL - not supported

                ecx = 0;
                edx = 0; // mostly speculation control
            } else {
                eax = 0;
                ebx = 0;
                ecx = 0;
                edx = 0;
            }
        }
        0xD => {
            // XSAVE: only x87 + SSE + AVX are virtualized.
            const SUPPORTED_XCR0: u32 = 0x7; // x87 | SSE | AVX
            const XSAVE_SIZE: u32 = 832; // 512 (legacy) + 64 (header) + 256 (AVX)

            match subleaf {
                0 => {
                    eax = SUPPORTED_XCR0;
                    ebx = XSAVE_SIZE; // Size for current XCR0
                    ecx = XSAVE_SIZE; // Max size for all supported
                    edx = 0; // High 32 bits of XCR0
                }
                1 => {
                    // No XSAVES.
                    eax = 0;
                    ebx = 0;
                    ecx = 0;
                    edx = 0;
                }
                2 => {
                    // AVX state (YMM_Hi128)
                    eax = 256; // Size
                    ebx = 576; // Offset (after legacy + header)
                    ecx = 0; // Flags: in XCR0, not XSS
                    edx = 0;
                }
                _ => {
                    eax = 0;
                    ebx = 0;
                    ecx = 0;
                    edx = 0;
                }
            }
        }
        0xB | 0x1F => {
            // Extended topology (0xB / 0x1F). EDX = x2APIC ID, which must
            // match CPUID.01H EBX[31:24] (SDM 12.12.8.1).
            edx = 0; // Virtual APIC ID = 0
            if subleaf == 0 || subleaf == 1 {
                ebx = 0; // No logical processors at this level
                ecx = (ecx & 0xFFFFFF00) | subleaf;
            } else {
                // No more levels.
                eax = 0;
                ebx = 0;
                ecx = subleaf;
                edx = 0;
            }
        }
        0x80000000 => {
            if eax < 0x80000004 {
                eax = 0x80000004; // Support brand string
            }
        }
        0x80000002..=0x80000004 => {
            let brand = b"Bedrock VM CPU  ";
            // SAFETY: brand is a 16-byte array, the same size as [u32; 4]; the
            // transmute reinterprets the bytes as little-endian u32s.
            let brand_dwords: &[u32; 4] = unsafe { core::mem::transmute(brand) };
            if leaf == 0x80000002 {
                eax = brand_dwords[0];
                ebx = brand_dwords[1];
                ecx = brand_dwords[2];
                edx = brand_dwords[3];
            } else {
                eax = 0x20202020; // Spaces
                ebx = 0x20202020;
                ecx = 0x20202020;
                edx = 0x00000000;
            }
        }
        0xA => {
            // Architectural PerfMon: report no PMU so the guest won't use RDPMC
            // (EAX[7:0]=0, SDM Vol 1 Table 21-30).
            eax = 0;
            ebx = 0;
            ecx = 0;
            edx = 0;
        }
        0x15 => {
            // TSC/crystal clock: TSC freq = ECX * EBX / EAX, so report
            // EAX=EBX=1 and ECX = configured TSC frequency (Hz).
            let tsc_freq = ctx.state().tsc_frequency;
            eax = 1;
            ebx = 1;
            ecx = (tsc_freq & 0xFFFFFFFF) as u32;
            edx = 0;
        }
        0x80000008 => {
            ecx &= 0xFFFFFF00; // Report single core
        }
        0x16 => {
            // Processor frequency (MHz): fixed, since host values vary with
            // power state.
            eax = 3000; // 3.0 GHz base frequency
            ebx = 5800; // 5.8 GHz max frequency
            ecx = 100; // 100 MHz bus frequency
            edx = 0;
        }
        _ => {
            // Unhandled leaves return 0 so no host-specific values leak.
            eax = 0;
            ebx = 0;
            ecx = 0;
            edx = 0;
        }
    }

    {
        let gprs = &mut ctx.state_mut().gprs;
        gprs.rax = u64::from(eax);
        gprs.rbx = u64::from(ebx);
        gprs.rcx = u64::from(ecx);
        gprs.rdx = u64::from(edx);
    }

    if let Err(e) = advance_rip(ctx) {
        return ExitHandlerResult::Error(e);
    }

    ExitHandlerResult::Continue
}

/// Execute CPUID instruction.
#[cfg(target_arch = "x86_64")]
pub(super) fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    // SAFETY: CPUID only reads processor identification data. RBX is
    // saved/restored because it is callee-saved and CPUID clobbers it.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            ebx_out = out(reg) ebx,
            lateout("edx") edx,
            options(nostack),
        );
    }
    (eax, ebx, ecx, edx)
}

/// Mock CPUID for non-x86_64 targets (for testing).
#[cfg(not(target_arch = "x86_64"))]
pub(super) fn cpuid(_leaf: u32, _subleaf: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}
