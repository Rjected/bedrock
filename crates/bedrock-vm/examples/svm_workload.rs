// SPDX-License-Identifier: GPL-2.0
//! Run identical memory-writing integer code natively and in an AMD SVM guest.
//! The timed interval excludes VM construction and buffer initialization.
use bedrock_vm::{Cr3, Regs, Vm};
use std::time::Instant;

const WORDS: usize = 4096;
const DATA_GPA: usize = 0x10000;

core::arch::global_asm!(
    r#"
    .section .text.bedrock_workload,"ax",@progbits
    .global bedrock_workload_start
    .type bedrock_workload_start,@function
bedrock_workload_start:
    xor eax, eax
    mov r8, rsi
    mov rdx, rdi
1:
    mov ecx, 4096
    mov rdi, rdx
2:
    mov r9, qword ptr [rdi]
    xor r9, rax
    rol r9, 13
    add r9, -1640531527
    mov qword ptr [rdi], r9
    xor rax, r9
    add rdi, 8
    dec rcx
    jnz 2b
    dec r8
    jnz 1b
    .global bedrock_workload_end
bedrock_workload_end:
    ret
    .size bedrock_workload_start, .-bedrock_workload_start
"#
);

unsafe extern "C" {
    fn bedrock_workload_start(data: *mut u64, rounds: u64) -> u64;
    fn bedrock_workload_end();
}

fn initial_data() -> Vec<u64> {
    (0..WORDS)
        .map(|i| (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .collect()
}

fn guest_run(
    rounds: u64,
    original: &[u64],
    expected: &[u64],
) -> Result<(f64, u64, u64, u64, u64, u64), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let start = bedrock_workload_start as *const () as usize;
    let end = bedrock_workload_end as *const () as usize;
    assert!(end > start && end - start < 4096);
    // SAFETY: Both labels delimit the same read-only assembly section.
    let body = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };
    let mut code = body.to_vec();
    code.extend([0x49, 0x89, 0xc2]); // mov r10,rax: preserve checksum.
    code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]); // xor eax,eax; vmmcall.
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    for (i, word) in original.iter().enumerate() {
        let offset = DATA_GPA + i * 8;
        vm.memory_mut()?[offset..offset + 8].copy_from_slice(&word.to_le_bytes());
    }
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rdi = DATA_GPA as u64;
    regs.gprs.rsi = rounds;
    vm.set_regs(&regs)?;
    let start = Instant::now();
    let (checksum, instructions) = loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258, "unexpected guest exit");
        break (vm.get_regs()?.gprs.r10, exit.emulated_tsc);
    };
    let seconds = start.elapsed().as_secs_f64();
    let memory = vm.memory()?;
    for (i, word) in expected.iter().enumerate() {
        let offset = DATA_GPA + i * 8;
        let actual = u64::from_le_bytes(memory[offset..offset + 8].try_into()?);
        assert_eq!(actual, *word, "word {i} differs");
    }
    let stats = vm.get_exit_stats()?;
    Ok((
        seconds,
        checksum,
        instructions,
        stats.total_exit_count(),
        stats.ept_violation.count,
        stats.mtf.count,
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 3 {
        return Err("usage: svm_workload [ROUNDS] [SAMPLES]".into());
    }
    let rounds: u64 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(64);
    let samples: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(3);
    assert!(rounds > 0 && samples > 0);
    let mut native_times = Vec::with_capacity(samples);
    let mut guest_times = Vec::with_capacity(samples);
    let original = initial_data();
    for _ in 0..samples {
        let mut native = original.clone();
        let start = Instant::now();
        // SAFETY: The assembly writes only the WORDS entries at `native`.
        let native_checksum = unsafe { bedrock_workload_start(native.as_mut_ptr(), rounds) };
        let native_seconds = start.elapsed().as_secs_f64();
        let (guest_seconds, guest_checksum, instructions, exits, npt_exits, mtf_exits) =
            guest_run(rounds, &original, &native)?;
        assert_eq!(guest_checksum, native_checksum, "checksum mismatch");
        assert_eq!(instructions, 5 + rounds * (WORDS as u64 * 9 + 4));
        println!(
            "SVM_WORKLOAD_SAMPLE rounds={rounds} instructions={instructions} native_seconds={native_seconds:.6} guest_seconds={guest_seconds:.6} exits={exits} npt_exits={npt_exits} mtf_exits={mtf_exits} checksum={guest_checksum:016x}"
        );
        native_times.push(native_seconds);
        guest_times.push(guest_seconds);
    }
    native_times.sort_by(f64::total_cmp);
    guest_times.sort_by(f64::total_cmp);
    let native = native_times[samples / 2];
    let guest = guest_times[samples / 2];
    println!(
        "SVM_WORKLOAD_PASS rounds={rounds} words={WORDS} samples={samples} native_seconds={native:.6} guest_seconds={guest:.6} slowdown={:.2}x",
        guest / native
    );
    Ok(())
}
