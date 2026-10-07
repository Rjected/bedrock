// SPDX-License-Identifier: GPL-2.0
//! Native instruction-throughput benchmark with an exact stop inside a block.
use bedrock_vm::{
    load_kernel, Cr3, LinuxBootConfig, RdrandConfig, Regs, SegmentRegister, Vm, VmBuilder,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "native" {
        return native_loop_comparison();
    }
    if args.len() == 3 {
        return linux_checkpoint(&args[1], &args[2]);
    }
    if args.len() != 1 {
        return Err("Usage: svm_bench [native | VMLINUX INITRD]".into());
    }
    test_repeat()?;
    test_self_modifying()?;
    test_debug_registers()?;
    test_hardware_loop()?;
    const LOOPS: u16 = 4096;
    const EXPECTED: u64 = 1 + LOOPS as u64 * 66 + 1;
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let mut code = vec![0xb9]; // mov cx,LOOPS
    code.extend(LOOPS.to_le_bytes());
    code.extend([0x90; 64]);
    code.extend([0x49, 0x75, 0xbd]); // dec cx; jnz back over 64 NOPs
    code.extend([0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(32))?;
    let start = Instant::now();
    let mut stopped = false;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        if exit.exit_reason == 259 {
            assert!(!stopped);
            assert_eq!(exit.emulated_tsc, 32);
            assert_eq!(vm.get_regs()?.rip, 0x1003 + 31);
            vm.set_stop_at_tsc(None)?;
            stopped = true;
            continue;
        }
        assert_eq!(exit.exit_reason, 258, "unexpected guest exit");
        assert_eq!(
            exit.emulated_tsc, EXPECTED,
            "instruction accounting changed"
        );
        let regs = vm.get_regs()?;
        assert_eq!(regs.gprs.rcx & 0xffff, 0);
        assert_eq!(regs.rflags & ((1 << 8) | (1 << 16)), 0);
        assert!(stopped);
        let seconds = start.elapsed().as_secs_f64();
        println!("SVM_BENCH_PASS instructions={EXPECTED} seconds={seconds:.6} instructions_per_second={:.0}", EXPECTED as f64 / seconds);
        break;
    }
    Ok(())
}

// The same register-only loop executes natively and in a long-mode guest.
// This measures verified-loop throughput, not Linux or general guest overhead.
fn native_loop_comparison() -> Result<(), Box<dyn std::error::Error>> {
    const ITERATIONS: u64 = 10_000_000;
    let mut native_times = Vec::new();
    let mut guest_times = Vec::new();
    for _ in 0..3 {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let mut code = vec![0x90; 32];
        code.extend([0x48, 0xff, 0xc9, 0x75, 0xdb]); // dec rcx; jnz start
        code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]);
        vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rcx = ITERATIONS;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        let start = Instant::now();
        // SAFETY: No memory or stack accesses; RCX and flags are declared
        // clobbered. The loop terminates after ITERATIONS decrements.
        unsafe {
            core::arch::asm!(
                ".p2align 4", "2:", ".rept 32", "nop", ".endr",
                "dec rcx", "jnz 2b",
                inout("rcx") ITERATIONS => _, options(nomem, nostack)
            );
        }
        native_times.push(start.elapsed().as_secs_f64());
        let start = Instant::now();
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, ITERATIONS * 34 + 1);
            assert_eq!(vm.get_regs()?.gprs.rcx, 0);
            break;
        }
        guest_times.push(start.elapsed().as_secs_f64());
    }
    println!("SVM_NATIVE_LOOP_SAMPLES native={native_times:?} guest={guest_times:?}");
    native_times.sort_by(f64::total_cmp);
    guest_times.sort_by(f64::total_cmp);
    println!(
        "SVM_NATIVE_LOOP_PASS instructions={} native_seconds={:.6} guest_seconds={:.6} overhead_percent={:.2}",
        ITERATIONS * 34, native_times[1], guest_times[1],
        (guest_times[1] / native_times[1] - 1.0) * 100.0
    );
    Ok(())
}

fn test_repeat() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let mut code = vec![0x48, 0xb9];
    code.extend(10_000u64.to_le_bytes());
    code.extend([0x48, 0xbf]);
    code.extend(0x7000u64.to_le_bytes());
    code.extend([
        0xb8, 0xa5, 0, 0, 0, 0xf3, 0xaa, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ]);
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(17))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, 17);
        break;
    }
    let r = vm.get_regs()?;
    assert_eq!(r.rip, 0x1019);
    assert_eq!(r.gprs.rcx, 9986);
    assert_eq!(r.gprs.rdi, 0x700e);
    assert!(vm.memory()?[0x7000..0x700e].iter().all(|&b| b == 0xa5));
    assert_eq!(vm.memory()?[0x700e], 0);
    vm.set_stop_at_tsc(None)?;
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    let parent_hash = hash.finish();
    let mut results = Vec::new();
    for _ in 0..2 {
        let child = vm.fork()?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 || exit.exit_reason == 262 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 10_004);
            let r = child.get_regs()?;
            assert_eq!(r.gprs.rcx, 0);
            assert_eq!(r.gprs.rdi, 0x7000 + 10_000);
            results.push((r.rip, r.rflags, r.gprs.rax, r.gprs.rcx, r.gprs.rdi));
            break;
        }
    }
    assert_eq!(results[0], results[1]);
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    assert_eq!(hash.finish(), parent_hash);
    println!("SVM_REP_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_hardware_loop() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let code = [
        0xb9, 0x60, 0xea, 0x90, 0x90, 0x90, 0x90, 0x49, 0x75, 0xf9, 0x66, 0x31, 0xc0, 0x0f, 0x01,
        0xd9,
    ];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(200_000))?;
    let start = Instant::now();
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, 200_000);
        let r = vm.get_regs()?;
        assert_eq!(r.rip, 0x1004);
        assert_eq!(r.gprs.rcx & 0xffff, 26667);
        assert_eq!(r.rflags & ((1 << 8) | (1 << 16)), 0);
        break;
    }
    vm.set_stop_at_tsc(None)?;
    let mut children = Vec::new();
    for _ in 0..2 {
        let child = vm.fork()?;
        child.set_stop_at_tsc(Some(300_000))?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 259);
            assert_eq!(exit.emulated_tsc, 300_000);
            let r = child.get_regs()?;
            assert_eq!(r.rip, 0x1008);
            assert_eq!(r.gprs.rcx & 0xffff, 10000);
            break;
        }
        child.set_stop_at_tsc(None)?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 360_002);
            let r = child.get_regs()?;
            children.push((exit.emulated_tsc, r.rip, r.rflags, r.gprs.rax, r.gprs.rcx));
            break;
        }
    }
    assert_eq!(children[0], children[1]);
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 360_002);
        assert_eq!(vm.get_regs()?.gprs.rcx & 0xffff, 0);
        break;
    }
    println!(
        "SVM_PMU_LOOP_DEADLINE_PASS instructions=360002 seconds={:.6}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn test_self_modifying() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
    let code = [
        0xc6, 0x06, 0x00, 0x11, 0x0f, 0xc6, 0x06, 0x01, 0x11, 0xc7, 0xc6, 0x06, 0x02, 0x11, 0xf0,
        0xe9, 0xee, 0x00,
    ];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let tail = [
        0x90, 0x90, 0x90, 0x89, 0xc3, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ];
    vm.memory_mut()?[0x1100..0x1100 + tail.len()].copy_from_slice(&tail);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        if exit.exit_reason == 57 {
            vm.set_rdrand_value(0x1234)?;
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        // Userspace RNG retirement is excluded by the reference SVM counter.
        assert_eq!(exit.emulated_tsc, 6);
        assert_eq!(vm.get_regs()?.gprs.rbx & 0xffff, 0x1234);
        break;
    }
    println!("SVM_SELF_MODIFYING_RNG_PASS");
    Ok(())
}

fn test_debug_registers() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let code = [
        0x66, 0xb8, 0x00, 0x04, 0, 0, // mov eax,0x400
        0x0f, 0x23, 0xf8, // mov dr7,rax
        0x66, 0xba, 0xf0, 0x0f, 0xff, 0xff, // mov edx,0xffff0ff0
        0x0f, 0x23, 0xf2, // mov dr6,rdx
        0x90, // A hypervisor TF step must not change logical DR6.
        0x0f, 0x21, 0xf3, // mov rbx,dr6
        0x66, 0xba, 0x34, 0x12, 0, 0, // mov edx,0x1234
        0x0f, 0x23, 0xc2, // mov dr0,rdx
        0x0f, 0x21, 0xc6, // mov rsi,dr0
        0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 10);
        let r = vm.get_regs()?;
        assert_eq!(r.gprs.rbx, 0xffff0ff0);
        assert_eq!(r.gprs.rsi, 0x1234);
        break;
    }
    println!("SVM_DEBUG_REGISTER_ISOLATION_PASS");
    Ok(())
}

fn linux_checkpoint(kernel: &str, initrd: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = VmBuilder::new()
        .memory_mb(128)
        .tsc_frequency(100_000_000)
        .build()?;
    let kernel = std::fs::read(kernel)?;
    let initrd = std::fs::read(initrd)?;
    let (entry, end) = load_kernel(vm.memory_mut()?, &kernel)?;
    vm.setup_linux_boot(
        &LinuxBootConfig::new(entry, end)
            .cmdline("console=ttyS0 nopti nokaslr mitigations=off audit=0")
            .initramfs(&initrd),
    )?;
    vm.set_stop_at_tsc(Some(1_000_000))?;
    let start = Instant::now();
    loop {
        if start.elapsed().as_secs() >= 10 {
            return Err("Linux checkpoint exceeded 10 seconds".into());
        }
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259, "unexpected Linux guest exit");
        assert_eq!(exit.emulated_tsc, 1_000_000);
        let seconds = start.elapsed().as_secs_f64();
        let stats = vm.get_exit_stats()?;
        println!(
            "{}",
            bedrock_vm::ExitStatsReport {
                stats: &stats,
                wall_clock: start.elapsed()
            }
        );
        let mut hash = DefaultHasher::new();
        hash.write(vm.memory()?);
        println!(
            "SVM_LINUX_CHECKPOINT_PASS seconds={seconds:.6} memory_hash={:016x}",
            hash.finish()
        );
        let r = vm.get_regs()?;
        let g = r.gprs;
        println!(
            "REGISTERS {:x?}",
            [
                r.rip,
                r.rflags,
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
                r.control_regs.cr0.bits(),
                r.control_regs.cr2.0,
                r.control_regs.cr3.bits(),
                r.control_regs.cr4.bits()
            ]
        );
        break;
    }
    Ok(())
}
