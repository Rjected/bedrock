// SPDX-License-Identifier: GPL-2.0
//! Boot guest/svm-test.c, then compare two executions forked from its snapshot.
use bedrock_vm::events::EventKind;
use bedrock_vm::{
    load_kernel, EventCategories, EventConfig, EventStream, ExitKind, LinuxBootConfig,
    RdrandConfig, Vm, VmBuilder,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::Hasher,
    time::{Duration, Instant},
};
#[path = "support/svm_tables.rs"]
mod svm_tables;

fn run_until(vm: &mut Vm, snapshot: bool) -> Result<u64, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let mut progress = start;
    loop {
        if start.elapsed() > Duration::from_secs(600) {
            return Err("Linux integration test timed out".into());
        }
        let exit = match vm.run() {
            Ok(exit) => exit,
            Err(error) => {
                let registers = vm.get_regs()?;
                eprintln!(
                    "SVM Linux failure after {:.3}s: rip={:#x}, rax={:#x}, rcx={:#x}",
                    start.elapsed().as_secs_f64(),
                    registers.rip,
                    registers.gprs.rax,
                    registers.gprs.rcx
                );
                return Err(error.into());
            }
        };
        // RUN has returned, so registers and memory are no longer changing.
        if progress.elapsed() >= Duration::from_secs(10) {
            let registers = vm.get_regs()?;
            let tables =
                svm_tables::page_table_count(vm.memory()?, registers.control_regs.cr3.bits());
            eprintln!(
                "SVM_LINUX_PROGRESS seconds={:.3} tsc={} rip={:#x} guest_tables={tables:?}",
                start.elapsed().as_secs_f64(),
                exit.emulated_tsc,
                registers.rip
            );
            progress = Instant::now();
        }
        if let Some(buffer) = vm.event_buffer() {
            for record in EventStream::new(&buffer[..exit.event_len as usize]) {
                if record.kind() == EventKind::Serial.as_u16() {
                    print!("{}", String::from_utf8_lossy(record.payload));
                }
            }
        }
        match exit.kind() {
            ExitKind::Continue | ExitKind::EventBufferFull | ExitKind::FeedbackBufferRegistered => {
            }
            ExitKind::VmcallSnapshot { .. } if snapshot => return Ok(exit.emulated_tsc),
            ExitKind::VmcallShutdown if !snapshot => return Ok(exit.emulated_tsc),
            kind => return Err(format!("Unexpected guest exit: {kind:?}").into()),
        }
    }
}

fn memory_hash(vm: &mut Vm) -> Result<u64, Box<dyn std::error::Error>> {
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    Ok(hash.finish())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let repeat = args.len() == 4 && args[3] == "repeat";
    if args.len() != 3 && !repeat {
        return Err("Usage: svm_linux VMLINUX SVM_TEST_INITRD [repeat]".into());
    }
    let kernel = std::fs::read(&args[1])?;
    let initrd = std::fs::read(&args[2])?;
    let first = run_linux(&kernel, &initrd)?;
    if repeat {
        let second = run_linux(&kernel, &initrd)?;
        assert_eq!(first, second, "Fresh Linux boots diverged");
        println!("SVM_LINUX_FRESH_REPLAY_PASS");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct RootReplay {
    snapshot_tsc: u64,
    parent_hash: u64,
    snapshot_registers: [u64; 18],
    children: Vec<(u64, Vec<u8>, [u64; 18])>,
}

fn run_linux(kernel: &[u8], initrd: &[u8]) -> Result<RootReplay, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let mut root = VmBuilder::new()
        .memory_mb(128)
        .tsc_frequency(100_000_000)
        .rdrand(RdrandConfig::seeded_rng(42))
        .build()?;
    root.set_event_config(&EventConfig::enabled(EventCategories::SERIAL))?;
    let (entry, end) = load_kernel(root.memory_mut()?, &kernel)?;
    root.setup_linux_boot(
        &LinuxBootConfig::new(entry, end)
            .cmdline("console=ttyS0 nopti nokaslr mitigations=off audit=0")
            .initramfs(&initrd),
    )?;
    let snapshot_tsc = run_until(&mut root, true)?;
    println!("SVM_LINUX_BOOT_PASS snapshot_tsc={snapshot_tsc}");
    let parent_hash = memory_hash(&mut root)?;
    let snapshot_registers = register_signature(root.get_regs()?);
    let parent_id = root.get_vm_id()?;
    let mut results = Vec::new();
    for _ in 0..2 {
        let mut child = VmBuilder::new()
            .forked_from(parent_id)
            .rdrand(RdrandConfig::seeded_rng(42))
            .build()?;
        let tsc = run_until(&mut child, false)?;
        child.map_feedback_buffer()?;
        let bytes = child.feedback_buffer().ok_or("Guest report is missing")?;
        let report = bytes[..64].to_vec();
        assert_eq!(
            u64::from_le_bytes(report[48..56].try_into()?),
            0,
            "clock_gettime failed"
        );
        assert_eq!(
            u64::from_le_bytes(report[56..64].try_into()?),
            16,
            "getrandom failed"
        );
        assert!(tsc > snapshot_tsc);
        let registers = register_signature(child.get_regs()?);
        results.push((tsc, report, registers));
    }
    assert_eq!(
        results[0], results[1],
        "Linux forks diverged in clock, randomness, registers or instruction count"
    );
    assert_eq!(
        memory_hash(&mut root)?,
        parent_hash,
        "Linux child modified parent memory"
    );
    println!(
        "SVM_LINUX_FORK_REPLAY_PASS shutdown_tsc={} report={:02x?}",
        results[0].0, results[0].1
    );
    println!(
        "SVM_LINUX_ROOT_PASS seconds={:.6} snapshot_tsc={} memory_hash={:016x}",
        start.elapsed().as_secs_f64(),
        snapshot_tsc,
        parent_hash
    );
    Ok(RootReplay {
        snapshot_tsc,
        parent_hash,
        snapshot_registers,
        children: results,
    })
}

fn register_signature(regs: bedrock_vm::Regs) -> [u64; 18] {
    let g = regs.gprs;
    [
        regs.rip,
        regs.rflags,
        g.rax,
        g.rbx,
        g.rcx,
        g.rdx,
        g.rsi,
        g.rdi,
        g.rbp,
        g.rsp,
        g.r8,
        g.r9,
        g.r10,
        g.r11,
        g.r12,
        g.r13,
        g.r14,
        g.r15,
    ]
}
