// SPDX-License-Identifier: GPL-2.0
//! Native instruction-throughput benchmark with an exact stop inside a block.
use bedrock_vm::{
    load_kernel, Cr3, Idtr, LinuxBootConfig, RdrandConfig, Regs, SegmentRegister, Vm, VmBuilder,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::time::{Duration, Instant};
#[path = "support/svm_tables.rs"]
mod svm_tables;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "native" {
        return native_loop_comparison(false);
    }
    if args.len() == 2 && args[1] == "native-branches" {
        return native_loop_comparison(true);
    }
    if args.len() == 2 && args[1] == "guarded-loops" {
        return test_page_loops(true, false);
    }
    if args.len() == 2 && args[1] == "loop-deadlines" {
        return test_counted_loop_deadlines();
    }
    if args.len() == 2 && args[1] == "page-loops" {
        return test_page_loops(false, false);
    }
    if args.len() == 2 && args[1] == "rep" {
        return test_repeat();
    }
    if args.len() == 2 && args[1] == "rep-proof" {
        return test_rep_cached_code_write();
    }
    if args.len() == 2 && args[1] == "control-flow" {
        for _ in 0..64 {
            test_hardware_control_flow()?;
        }
        return Ok(());
    }
    if args.len() == 2 && args[1] == "stores" {
        return test_paged_stores();
    }
    if args.len() == 2 && args[1] == "rmw-stores" {
        return test_read_modify_write_stores();
    }
    let repeat_checkpoint = args.len() == 5 && args[4] == "repeat";
    if args.len() == 3 || args.len() == 4 || repeat_checkpoint {
        let target = args
            .get(3)
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(1_000_000);
        if let Ok(start) = std::env::var("BEDROCK_CHECKPOINT_FORK_START") {
            if !repeat_checkpoint {
                return Err("Fork checkpoint replay requires repeat".into());
            }
            return linux_fork_checkpoints(&args[1], &args[2], start.parse()?, target);
        }
        let first = linux_checkpoint(&args[1], &args[2], target)?;
        if repeat_checkpoint {
            let second = linux_checkpoint(&args[1], &args[2], target)?;
            assert_eq!(first.len(), second.len());
            for (first, second) in first.iter().zip(&second) {
                assert_eq!(
                    first, second,
                    "Fresh Linux checkpoints diverged at {}",
                    first.tsc
                );
                println!("SVM_LINUX_FRESH_CHECKPOINT_MATCH tsc={}", first.tsc);
            }
            println!("SVM_LINUX_FRESH_CHECKPOINT_PASS tsc={target}");
        }
        return Ok(());
    }
    if args.len() != 1 {
        return Err(
            "Usage: svm_bench [native | native-branches | control-flow | rep | rep-proof | stores | rmw-stores | page-loops | guarded-loops | loop-deadlines | VMLINUX INITRD [INSTRUCTIONS [repeat]]]"
                .into(),
        );
    }
    test_repeat()?;
    test_rep_cached_code_write()?;
    test_paged_stores()?;
    test_read_modify_write_stores()?;
    test_self_modifying()?;
    test_guarded_code_and_translation_writes()?;
    test_page_rng_breakpoints()?;
    test_page_fetch_rng()?;
    test_decoded_branch_exits()?;
    test_data_translation_write()?;
    test_debug_registers()?;
    test_hardware_loop()?;
    test_hardware_control_flow()?;
    test_endpoint_deadline()?;
    test_forward_deadline()?;
    test_forward_stores()?;
    test_page_loops(false, false)?;
    test_page_loops(true, false)?;
    test_page_loops(true, true)?;
    test_counted_loop_deadlines()?;
    test_counter_idt_shadow()?;
    test_native_pushf_flags()?;
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
fn native_loop_comparison(branched: bool) -> Result<(), Box<dyn std::error::Error>> {
    const ITERATIONS: u64 = 10_000_000;
    const SAMPLES: usize = 9;
    let mut native_times = Vec::new();
    let mut guest_times = Vec::new();
    let mut native_cpu_times = Vec::new();
    let mut guest_cpu_times = Vec::new();
    for _ in 0..SAMPLES {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let mut code = if branched {
            let mut code = vec![0xf6, 0xc1, 1, 0x74, 8]; // test cl,1; jz skip
            code.extend([0x90; 16]);
            code.extend([0x48, 0xff, 0xc9, 0x75, 0xe6]);
            code
        } else {
            let mut code = vec![0x90; 32];
            code.extend([0x48, 0xff, 0xc9, 0x75, 0xdb]);
            code
        };
        code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]);
        vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rcx = ITERATIONS;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        let cpu_start = thread_cpu_seconds()?;
        let start = Instant::now();
        // SAFETY: No memory or stack accesses; RCX and flags are declared
        // clobbered. The loop terminates after ITERATIONS decrements.
        unsafe {
            if branched {
                core::arch::asm!(
                ".p2align 6", "2:", "test cl,1", "jz 3f",
                    ".rept 8", "nop", ".endr", "3:",
                    ".rept 8", "nop", ".endr", "dec rcx", "jnz 2b",
                    inout("rcx") ITERATIONS => _, options(nomem, nostack)
                );
            } else {
                core::arch::asm!(
                ".p2align 6", "2:", ".rept 32", "nop", ".endr",
                    "dec rcx", "jnz 2b",
                    inout("rcx") ITERATIONS => _, options(nomem, nostack)
                );
            }
        }
        native_times.push(start.elapsed().as_secs_f64());
        native_cpu_times.push(thread_cpu_seconds()? - cpu_start);
        let cpu_start = thread_cpu_seconds()?;
        let start = Instant::now();
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(
                exit.emulated_tsc,
                ITERATIONS * if branched { 16 } else { 34 } + 1
            );
            assert_eq!(vm.get_regs()?.gprs.rcx, 0);
            break;
        }
        guest_times.push(start.elapsed().as_secs_f64());
        guest_cpu_times.push(thread_cpu_seconds()? - cpu_start);
    }
    println!("SVM_NATIVE_LOOP_SAMPLES native={native_times:?} guest={guest_times:?}");
    println!("SVM_NATIVE_LOOP_CPU_SAMPLES native={native_cpu_times:?} guest={guest_cpu_times:?}");
    let mut paired_cpu_overheads: Vec<_> = native_cpu_times
        .iter()
        .zip(&guest_cpu_times)
        .map(|(native, guest)| (guest / native - 1.0) * 100.0)
        .collect();
    paired_cpu_overheads.sort_by(f64::total_cmp);
    native_times.sort_by(f64::total_cmp);
    guest_times.sort_by(f64::total_cmp);
    native_cpu_times.sort_by(f64::total_cmp);
    guest_cpu_times.sort_by(f64::total_cmp);
    println!(
        "SVM_NATIVE_LOOP_PASS branched={branched} instructions={} native_seconds={:.6} guest_seconds={:.6} overhead_percent={:.2}",
        ITERATIONS * if branched { 16 } else { 34 }, native_times[SAMPLES / 2], guest_times[SAMPLES / 2],
        (guest_times[SAMPLES / 2] / native_times[SAMPLES / 2] - 1.0) * 100.0
    );
    println!("SVM_NATIVE_LOOP_CPU branched={branched} native_seconds={:.6} guest_seconds={:.6} median_pair_overhead_percent={:.2}",
        native_cpu_times[SAMPLES / 2], guest_cpu_times[SAMPLES / 2], paired_cpu_overheads[SAMPLES / 2]);
    Ok(())
}

fn thread_cpu_seconds() -> Result<f64, std::io::Error> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a writable timespec, and this clock needs no privileges.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9)
}

fn test_rep_cached_code_write() -> Result<(), Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    for reference in [true, false] {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
        for (address, entry) in [(0x3000, 0x4027u64), (0x4000, 0x5027), (0x5000, 0xe7)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        // Warm page 0x2000, then write RDRAND into it with REP MOVSB.
        // Re-enter at NOPs before the new opcode to exercise its cached scan.
        let code = [
            0xe8, 0xfb, 0x0f, 0, 0, // CALL 0x2000
            0xb9, 3, 0, 0, 0, // MOV ECX, 3
            0xbe, 0, 0xb0, 0, 0, // MOV ESI, 0xb000
            0xbf, 0x10, 0x20, 0, 0, // MOV EDI, 0x2010
            0xf3, 0xa4, // REP MOVSB
            0xe9, 0xed, 0x0f, 0, 0, // JMP 0x2008
        ];
        vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        vm.memory_mut()?[0x2000..0x2020].fill(0x90);
        vm.memory_mut()?[0x2000] = 0xc3;
        vm.memory_mut()?[0x2013..0x2018].copy_from_slice(&[0x31, 0xc0, 0x0f, 0x01, 0xd9]);
        // Reject the copying page for whole-page execution so REP uses its
        // bounded native executor instead of hitting an entry breakpoint.
        for address in [0x1800, 0x1820, 0x1840, 0x1860, 0x1880] {
            vm.memory_mut()?[address..address + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        vm.memory_mut()?[0xb000..0xb003].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        if reference {
            vm.set_single_step_range(0, 100)?;
        }
        let mut random_exits = 0;
        loop {
            let exit = vm.run()?;
            match exit.exit_reason {
                256 => continue,
                57 => {
                    assert_eq!(vm.get_regs()?.rip, 0x2010);
                    random_exits += 1;
                    vm.set_rdrand_value(0x1234)?;
                }
                258 => {
                    assert_eq!(
                        random_exits, 1,
                        "REP write must revoke the cached hazard-free scan"
                    );
                    let regs = vm.get_regs()?;
                    results.push((exit.emulated_tsc, regs.rip, regs.rflags, regs.gprs.rcx));
                    break;
                }
                reason => return Err(format!("Unexpected REP proof result: {reason}").into()),
            }
        }
    }
    assert_eq!(results[0], results[1]);
    println!("SVM_REP_CACHED_CODE_WRITE_PASS");
    Ok(())
}

fn test_hardware_control_flow() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let code = [
        0xb9, 0x60, 0xea, 0xf6, 0xc1, 1, 0x74, 1, 0x90, 0x90, 0x49, 0x75, 0xf6, 0x66, 0x31, 0xc0,
        0x0f, 0x01, 0xd9,
    ];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(200_000))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, 200_000);
        let r = vm.get_regs()?;
        assert_eq!(r.rip, 0x1009);
        assert_eq!(r.gprs.rcx & 0xffff, 23637);
        break;
    }
    vm.set_stop_at_tsc(None)?;
    let mut results = Vec::new();
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
            assert_eq!(r.gprs.rcx & 0xffff, 5455);
            break;
        }
        child.set_stop_at_tsc(None)?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 330_002);
            let r = child.get_regs()?;
            assert_eq!(r.gprs.rcx & 0xffff, 0);
            assert_eq!(r.rflags & ((1 << 8) | (1 << 16)), 0);
            results.push((r.rip, r.rflags, r.gprs.rax, r.gprs.rcx));
            break;
        }
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(vm.get_regs()?.gprs.rcx & 0xffff, 23637);
    println!("SVM_CONTROL_FLOW_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_endpoint_deadline() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    vm.memory_mut()?[0x1000..0x1005].copy_from_slice(&[0x90, 0x90, 0x0f, 0x01, 0xd9]);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rax = 1; // Snapshot must not happen before the precise stop.
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(2))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, 2);
        assert_eq!(vm.get_regs()?.rip, 0x1002);
        break;
    }
    vm.set_stop_at_tsc(None)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 260);
        assert_eq!(exit.emulated_tsc, 2);
        assert_eq!(vm.get_regs()?.rip, 0x1005);
        break;
    }
    println!("SVM_INTERCEPT_ENDPOINT_DEADLINE_PASS");
    Ok(())
}

fn test_forward_deadline() -> Result<(), Box<dyn std::error::Error>> {
    // The branch skips one MOV on the taken path. Test every deadline on
    // both paths, including a stop before the snapshot endpoint.
    for taken in [false, true] {
        let mut reference = None;
        for stop in 1..=4 {
            let mut vm = Vm::create(2 * 1024 * 1024)?;
            vm.memory_mut()?[0x1000..0x100a]
                .copy_from_slice(&[0x90, 0x74, 3, 0xbb, 0x34, 0x12, 0x90, 0x0f, 0x01, 0xd9]);
            let mut regs = Regs::real_mode();
            regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
            regs.rip = 0x1000;
            regs.gprs.rsp = 0x8000;
            regs.gprs.rax = 1;
            if taken {
                regs.rflags |= 1 << 6;
            }
            vm.set_regs(&regs)?;
            // The taken path has three instructions before the endpoint.
            let target = if taken { stop.min(3) } else { stop };
            vm.set_stop_at_tsc(Some(target))?;
            loop {
                let exit = vm.run()?;
                if exit.exit_reason == 256 {
                    continue;
                }
                assert_eq!(exit.exit_reason, 259);
                assert_eq!(exit.emulated_tsc, target);
                let rip = match (taken, target) {
                    (_, 1) => 0x1001,
                    (false, 2) => 0x1003,
                    (true, 2) | (false, 3) => 0x1006,
                    _ => 0x1007,
                };
                assert_eq!(vm.get_regs()?.rip, rip);
                break;
            }
            // Resume two forks and compare their final architectural state.
            for _ in 0..2 {
                let child = vm.fork()?;
                child.set_stop_at_tsc(None)?;
                loop {
                    let exit = child.run()?;
                    if exit.exit_reason == 256 {
                        continue;
                    }
                    assert_eq!(exit.exit_reason, 260);
                    assert_eq!(exit.emulated_tsc, if taken { 3 } else { 4 });
                    let r = child.get_regs()?;
                    assert_eq!(r.gprs.rbx, if taken { 0 } else { 0x1234 });
                    let result = (r.rip, r.rflags, r.gprs.rbx);
                    if let Some(expected) = reference {
                        assert_eq!(result, expected);
                    } else {
                        reference = Some(result);
                    }
                    break;
                }
            }
        }
    }
    println!("SVM_FORWARD_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_forward_stores() -> Result<(), Box<dyn std::error::Error>> {
    const VALUE: u64 = 0x123456789abcdef0;
    for taken in [false, true] {
        for stop in 1..=4 {
            let mut vm = Vm::create(2 * 1024 * 1024)?;
            for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
                vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
            }
            let code = [
                0x48, 0x89, 0x07, 0x74, 4, 0x48, 0x89, 0x47, 8, 0x90, 0x4c, 0x8b, 0x07, 0x4c, 0x8b,
                0x4f, 8, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
            ];
            vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
            let mut regs = Regs::long_mode();
            regs.control_regs.cr3 = Cr3::new(0x3000);
            regs.rip = 0x1000;
            regs.gprs.rsp = 0x8000;
            regs.gprs.rdi = 0x7000;
            regs.gprs.rax = VALUE;
            if taken {
                regs.rflags |= 1 << 6;
            }
            vm.set_regs(&regs)?;
            vm.set_stop_at_tsc(Some(stop))?;
            loop {
                let exit = vm.run()?;
                if exit.exit_reason == 256 {
                    continue;
                }
                assert_eq!(exit.exit_reason, 259);
                assert_eq!(exit.emulated_tsc, stop);
                let rip = match (taken, stop) {
                    (_, 1) => 0x1003,
                    (false, 2) => 0x1005,
                    (true, 2) | (false, 3) => 0x1009,
                    (true, 3) | (false, 4) => 0x100a,
                    _ => 0x100d,
                };
                assert_eq!(vm.get_regs()?.rip, rip);
                break;
            }
            assert_eq!(&vm.memory()?[0x7000..0x7008], &VALUE.to_le_bytes());
            let second = if !taken && stop >= 3 { VALUE } else { 0 };
            assert_eq!(&vm.memory()?[0x7008..0x7010], &second.to_le_bytes());
            let mut hash = DefaultHasher::new();
            hash.write(vm.memory()?);
            let parent = hash.finish();
            for _ in 0..2 {
                let child = vm.fork()?;
                child.set_stop_at_tsc(None)?;
                loop {
                    let exit = child.run()?;
                    if exit.exit_reason == 256 {
                        continue;
                    }
                    assert_eq!(exit.exit_reason, 258);
                    assert_eq!(exit.emulated_tsc, if taken { 6 } else { 7 });
                    let r = child.get_regs()?;
                    assert_eq!(r.gprs.r8, VALUE);
                    assert_eq!(r.gprs.r9, if taken { 0 } else { VALUE });
                    break;
                }
            }
            let mut hash = DefaultHasher::new();
            hash.write(vm.memory()?);
            assert_eq!(hash.finish(), parent);
        }
    }
    println!("SVM_FORWARD_STORE_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_native_pushf_flags() -> Result<(), Box<dyn std::error::Error>> {
    const LOOPS: u32 = 2048;
    const TARGET: u64 = 1 + 4 * LOOPS as u64;
    let mut expected = None;
    let mut flag_exits = [0; 2];
    for (index, reference) in [false, true].into_iter().enumerate() {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        for (address, entry) in [(0x3000, 0x4027u64), (0x4000, 0x5027), (0x5000, 0xe7)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        vm.memory_mut()?[0x1000..0x2000].fill(0x90);
        let mut code = vec![0xb9]; // mov ecx,LOOPS
        code.extend(LOOPS.to_le_bytes());
        code.extend([0x9c, 0x58, 0x48, 0xff, 0xc9, 0x75, 0xf9]); // pushfq; pop rax; dec rcx; jnz
        code.extend([0x0f, 0x01, 0xd9]);
        vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.rflags = 2;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        if reference {
            vm.set_single_step_range(0, TARGET)?;
        }
        vm.set_stop_at_tsc(Some(TARGET))?;
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 259);
            assert_eq!(exit.emulated_tsc, TARGET);
            break;
        }
        let r = vm.get_regs()?;
        assert_eq!((r.rip, r.gprs.rcx, r.gprs.rsp), (0x100c, 0, 0x8000));
        assert_eq!(
            r.gprs.rax & ((1 << 8) | (1 << 16)),
            0,
            "PUSHF leaked hypervisor flags"
        );
        let stack = u64::from_le_bytes(vm.memory()?[0x7ff8..0x8000].try_into()?);
        let observed = (r.gprs.rax, r.rflags, stack);
        if let Some(expected) = expected {
            assert_eq!(observed, expected);
        } else {
            expected = Some(observed);
        }
        flag_exits[index] = vm.get_exit_stats()?.other.count;
    }
    assert!(
        flag_exits[0] < flag_exits[1],
        "native PUSHF still trapped on every iteration"
    );
    println!(
        "SVM_NATIVE_PUSHF_FLAGS_PASS native_exits={} reference_exits={}",
        flag_exits[0], flag_exits[1]
    );
    Ok(())
}

fn test_counter_idt_shadow() -> Result<(), Box<dyn std::error::Error>> {
    const LOOPS: u32 = 65_536;
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let mut code = vec![0xb9]; // mov ecx,LOOPS
    code.extend(LOOPS.to_le_bytes());
    code.extend([0x90; 64]);
    code.extend([0xff, 0xc9, 0x75, 0xbc]); // dec ecx; jnz over NOPs
    code.extend([0x0f, 0x01, 0x0f, 0x31, 0xc0, 0x0f, 0x01, 0xd9]); // sidt [rdi]; xor; vmmcall
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.descriptor_tables.idtr = Idtr::new(0x9000, 0xfff);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rdi = 0x7000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(200_000))?;
    let mut stopped = false;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        let regs = vm.get_regs()?;
        let idt = regs.descriptor_tables.idtr;
        assert_eq!((idt.base, idt.limit), (0x9000, 0xfff));
        assert_eq!(regs.gprs.rsp, 0x8000, "PMI wrote a guest interrupt frame");
        if exit.exit_reason == 259 {
            assert!(!stopped);
            assert_eq!(exit.emulated_tsc, 200_000);
            vm.set_stop_at_tsc(None)?;
            stopped = true;
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 1 + u64::from(LOOPS) * 66 + 2);
        assert_eq!(regs.gprs.rcx, 0);
        assert!(stopped);
        let memory = vm.memory()?;
        assert_eq!(
            u16::from_le_bytes(memory[0x7000..0x7002].try_into()?),
            0xfff
        );
        assert_eq!(
            u64::from_le_bytes(memory[0x7002..0x700a].try_into()?),
            0x9000
        );
        break;
    }
    println!("SVM_COUNTER_IDT_SHADOW_PASS");
    Ok(())
}

fn test_page_loops(guarded: bool, counter_payload: bool) -> Result<(), Box<dyn std::error::Error>> {
    const VALUE: u64 = 0x123456789abcdef0;
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    // Six instructions per iteration: CALL, MOV store, LEA, RET, DEC, JNZ.
    // Stores traverse many data pages; their addresses change every iteration.
    let mut code = vec![
        0xe8, 21, 0, 0, 0, 0x48, 0xff, 0xc9, 0x75, 0xf6, 0x4c, 0x8b, 0x87,
    ];
    code.extend((-800_000i32).to_le_bytes());
    code.extend([
        0x4c, 0x8b, 0x4f, 0xf8, 0x31, 0xc0, 0x0f, 0x01, 0xd9, 0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f,
        8, 0xc3,
    ]);
    if guarded {
        code = vec![
            0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4, 0x4c, 0x8b, 0x87,
        ];
        code.extend((-800_000i32).to_le_bytes());
        code.extend([0x4c, 0x8b, 0x4f, 0xf8, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
        // Unreachable RNG bytes reject whole-page execution. The decoded
        // loop must accelerate independently of unrelated page contents.
        for offset in [0x1800, 0x1820, 0x1840, 0x1860, 0x1880] {
            vm.memory_mut()?[offset..offset + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
    }
    if counter_payload {
        code[2] = 0x0f;
    } // MOV [RDI], RCX cannot use a clamped counter.
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rdi = 0x10000;
    regs.gprs.rcx = 100_000;
    regs.gprs.rax = VALUE;
    vm.set_regs(&regs)?;
    let deadline = if guarded { 200_002 } else { 200_000 };
    vm.set_stop_at_tsc(Some(deadline))?;
    let start = Instant::now();
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, deadline);
        let r = vm.get_regs()?;
        assert_eq!(r.rip, if guarded { 0x1007 } else { 0x101d });
        assert_eq!(r.gprs.rcx, if guarded { 50000 } else { 66667 });
        assert_eq!(
            r.gprs.rdi,
            0x10000 + if guarded { 50001 * 8 } else { 33333 * 8 }
        );
        assert_eq!(r.gprs.rsp, if guarded { 0x8000 } else { 0x7ff8 });
        break;
    }
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    let parent = hash.finish();
    let mut results = Vec::new();
    for _ in 0..2 {
        let child = vm.fork()?;
        child.set_stop_at_tsc(None)?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, if guarded { 400_003 } else { 600_003 });
            let r = child.get_regs()?;
            assert_eq!(r.gprs.rdi, 0x10000 + 800_000);
            assert_eq!(r.gprs.rcx, 0);
            assert_eq!(r.gprs.rsp, 0x8000);
            assert_eq!(r.gprs.r8, if counter_payload { 100_000 } else { VALUE });
            assert_eq!(r.gprs.r9, if counter_payload { 1 } else { VALUE });
            results.push((r.rip, r.rflags, r.gprs.rdi, r.gprs.r8, r.gprs.r9));
            break;
        }
    }
    assert_eq!(results[0], results[1]);
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    assert_eq!(hash.finish(), parent);
    println!(
        "SVM_STORE_LOOP_FORK_PASS guarded={guarded} counter_payload={counter_payload} seconds={:.6}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn test_counted_loop_deadlines() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    for (register, width) in [(1, 64), (2, 64), (1, 32), (2, 32)] {
        for decrement_first in [false, true] {
            for original_count in [
                5u64,
                16,
                257,
                0x80000000,
                0x80000001,
                (1 << 63) + 1,
                u64::MAX,
            ] {
                for deadline in 4..20 {
                    let mut vm = Vm::create(2 * 1024 * 1024)?;
                    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)]
                    {
                        vm.memory_mut()?[address..address + 8]
                            .copy_from_slice(&entry.to_le_bytes());
                    }
                    let mut code = if decrement_first {
                        [
                            0x48, 0xff, 0xc9, 0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x75, 0xf4,
                        ]
                    } else {
                        [
                            0x48, 0x89, 0x07, 0x48, 0x8d, 0x7f, 8, 0x48, 0xff, 0xc9, 0x75, 0xf4,
                        ]
                    };
                    let decrement_offset = if decrement_first { 0 } else { 7 };
                    if register == 2 {
                        code[decrement_offset + 2] = 0xca;
                    }
                    let code: Vec<u8> = if width == 32 {
                        code.iter()
                            .enumerate()
                            .filter_map(|(i, &byte)| (i != decrement_offset).then_some(byte))
                            .collect()
                    } else {
                        code.to_vec()
                    };
                    let mut code = code;
                    let code_len = code.len();
                    code[code_len - 1] = (-(code_len as i8)) as u8;
                    vm.memory_mut()?[0x1000..0x1000 + code_len].copy_from_slice(&code);
                    vm.memory_mut()?[0x1800..0x1803].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
                    let mut regs = Regs::long_mode();
                    regs.control_regs.cr3 = Cr3::new(0x3000);
                    regs.rip = 0x1000;
                    regs.rflags = 0x203;
                    regs.gprs.rsp = 0x8000;
                    regs.gprs.rdi = 0x10000;
                    let original_count = if width == 32 {
                        0x100000000 | ((original_count as u32 as u64).max(5))
                    } else {
                        original_count
                    };
                    if register == 1 {
                        regs.gprs.rcx = original_count;
                    } else {
                        regs.gprs.rdx = original_count;
                    }
                    regs.gprs.rax = 0x123456789abcdef0;
                    vm.set_regs(&regs)?;
                    vm.set_stop_at_tsc(Some(deadline))?;
                    loop {
                        let exit = vm.run()?;
                        if exit.exit_reason == 256 {
                            continue;
                        }
                        assert_eq!(exit.exit_reason, 259);
                        assert_eq!(exit.emulated_tsc, deadline);
                        break;
                    }
                    let r = vm.get_regs()?;
                    let instruction = deadline % 4;
                    let loops = deadline / 4;
                    let decrements =
                        loops + u64::from(instruction > if decrement_first { 0 } else { 2 });
                    let advances =
                        loops + u64::from(instruction > if decrement_first { 2 } else { 1 });
                    let stores =
                        loops + u64::from(instruction > if decrement_first { 1 } else { 0 });
                    let mut offsets = if decrement_first {
                        [0, 3, 6, 10]
                    } else {
                        [0, 3, 7, 10]
                    };
                    if width == 32 {
                        for offset in &mut offsets {
                            if *offset > decrement_offset as u64 {
                                *offset -= 1;
                            }
                        }
                    }
                    assert_eq!(r.rip, 0x1000 + offsets[instruction as usize]);
                    let remaining = if width == 32 {
                        original_count as u32 as u64 - decrements
                    } else {
                        original_count - decrements
                    };
                    assert_eq!(
                        if register == 1 {
                            r.gprs.rcx
                        } else {
                            r.gprs.rdx
                        },
                        remaining
                    );
                    assert_eq!(r.gprs.rdi, 0x10000 + advances * 8);
                    let flags: u64;
                    unsafe {
                        if width == 32 {
                            core::arch::asm!("stc", "dec {value:e}", "pushfq", "pop {flags}",
                            value = inout(reg) remaining + 1 => _, flags = lateout(reg) flags);
                        } else {
                            core::arch::asm!("stc", "dec {value}", "pushfq", "pop {flags}",
                        value = inout(reg) original_count - decrements + 1 => _,
                        flags = lateout(reg) flags);
                        }
                    }
                    assert_eq!(r.rflags & 0x8d5, flags & 0x8d5);
                    for index in 0..stores as usize {
                        assert_eq!(
                            &vm.memory()?[0x10000 + index * 8..0x10008 + index * 8],
                            &regs.gprs.rax.to_le_bytes()
                        );
                    }
                    assert_eq!(
                        &vm.memory()?[0x10000 + stores as usize * 8..0x10008 + stores as usize * 8],
                        &[0; 8]
                    );
                }
            }
        }
    }
    println!(
        "SVM_COUNTED_LOOP_DEADLINES_PASS seconds={:.6}",
        start.elapsed().as_secs_f64()
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
    for replay in 0..2 {
        let child = vm.fork()?;
        if replay == 0 {
            child.set_single_step_range(17, 10_005)?;
        }
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
        let exits = child.get_exit_stats()?.total_exit_count();
        if replay != 0 {
            assert!(
                exits < 100,
                "REP copy should use bounded native chunks: {exits} exits"
            );
        }
        if replay == 0 {
            println!("SVM_REP_REFERENCE_EXITS count={exits}");
        } else {
            println!("SVM_REP_NATIVE_EXITS count={exits}");
        }
    }
    assert_eq!(results[0], results[1]);
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    assert_eq!(hash.finish(), parent_hash);
    println!("SVM_REP_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_paged_stores() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let mut code = vec![0x48, 0x89, 0x07];
    for displacement in (8..64).step_by(8) {
        code.extend([0x48, 0x89, 0x47, displacement]);
    }
    code.extend([0x48, 0x8d, 0x7f, 0x40]);
    for index in 0..8 {
        code.extend([0x4c, 0x8b, 0x47 | (index << 3), 0xc0 + index * 8]);
    }
    code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]);
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rdi = 0x7000;
    regs.gprs.rax = 0x123456789abcdef0;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(3))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        assert_eq!(exit.emulated_tsc, 3);
        assert_eq!(vm.get_regs()?.rip, 0x100b);
        break;
    }
    for offset in (0..24).step_by(8) {
        assert_eq!(
            &vm.memory()?[0x7000 + offset..0x7008 + offset],
            &regs.gprs.rax.to_le_bytes()
        );
    }
    assert_eq!(&vm.memory()?[0x7018..0x7040], &[0; 40]);
    vm.set_stop_at_tsc(None)?;
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    let parent_hash = hash.finish();
    let mut results = Vec::new();
    for _ in 0..2 {
        let child = vm.fork()?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 18);
            let r = child.get_regs()?;
            assert_eq!(r.gprs.rdi, 0x7040);
            for value in [
                r.gprs.r8, r.gprs.r9, r.gprs.r10, r.gprs.r11, r.gprs.r12, r.gprs.r13, r.gprs.r14,
                r.gprs.r15,
            ] {
                assert_eq!(value, regs.gprs.rax);
            }
            results.push((r.rip, r.rflags, r.gprs.rax, r.gprs.rdi));
            break;
        }
    }
    assert_eq!(results[0], results[1]);
    let mut hash = DefaultHasher::new();
    hash.write(vm.memory()?);
    assert_eq!(hash.finish(), parent_hash);
    println!("SVM_PAGED_STORE_DEADLINE_FORK_PASS");
    Ok(())
}

fn test_read_modify_write_stores() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let code = [
        0x48, 0xff, 0x07, // inc qword [rdi]
        0x48, 0x83, 0x6f, 8, 1, // sub qword [rdi+8],1
        0x0f, 0x94, 0x47, 16, // sete byte [rdi+16]
        0x0f, 0xb6, 0x5f, 16, // movzx ebx,byte [rdi+16]
        0x0f, 0x01, 0xd9, // vmmcall
    ];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    vm.memory_mut()?[0x7000..0x7008].copy_from_slice(&1u64.to_le_bytes());
    vm.memory_mut()?[0x7008..0x7010].copy_from_slice(&1u64.to_le_bytes());
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    regs.gprs.rdi = 0x7000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(2))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!((exit.exit_reason, exit.emulated_tsc), (259, 2));
        assert_eq!(vm.get_regs()?.rip, 0x1008);
        break;
    }
    assert_eq!(u64::from_le_bytes(vm.memory()?[0x7000..0x7008].try_into()?), 2);
    assert_eq!(u64::from_le_bytes(vm.memory()?[0x7008..0x7010].try_into()?), 0);
    assert_eq!(vm.memory()?[0x7010], 0);
    vm.set_stop_at_tsc(None)?;
    let parent = vm.memory()?.to_vec();
    let mut results = Vec::new();
    for reference in [false, true] {
        let child = vm.fork()?;
        if reference {
            child.set_single_step_range(2, 4)?;
        }
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!((exit.exit_reason, exit.emulated_tsc), (258, 4));
            let registers = child.get_regs()?;
            assert_eq!(registers.gprs.rbx, 1);
            results.push((registers.rip, registers.rflags));
            break;
        }
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(vm.memory()?, parent.as_slice());
    println!("SVM_RMW_STORE_DEADLINE_FORK_PASS");
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

fn test_guarded_code_and_translation_writes() -> Result<(), Box<dyn std::error::Error>> {
    for (translation, guarded) in [(false, true), (true, true), (true, false)] {
        let mut vm = Vm::create(4 * 1024 * 1024)?;
        vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
        for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let tail = [0x90, 0x90, 0x90, 0x89, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9];
        let mut code = vec![0x48, 0x89, 0x07];
        if !translation {
            code.extend([0x90, 0xeb, 8]);
            code.extend([0x90; 8]);
        }
        code.extend(tail);
        vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        // Also exercise page-wide execution without the rejection marker.
        if guarded {
            vm.memory_mut()?[0x1800..0x1803].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
        }
        if translation {
            vm.memory_mut()?[0x201000..0x201003].copy_from_slice(&[0x48, 0x89, 0x07]);
            let new_tail = [0x0f, 0xc7, 0xf0, 0x89, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9];
            vm.memory_mut()?[0x201003..0x20100d].copy_from_slice(&new_tail);
        }
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rsp = 0x8000;
        regs.gprs.rdi = if translation { 0x5000 } else { 0x100e };
        regs.gprs.rax = if translation {
            0x200087
        } else {
            0x0fc031c389f0c70f
        };
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
            assert_eq!(exit.emulated_tsc, if translation { 3 } else { 5 });
            assert_eq!(vm.get_regs()?.gprs.rbx, 0x1234);
            break;
        }
    }
    println!("SVM_GUARDED_CODE_TRANSLATION_RNG_PASS");
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

fn test_page_rng_breakpoints() -> Result<(), Box<dyn std::error::Error>> {
    for target in [0x1100u64, 0x1120, 0x1140, 0x1160, 0x201100] {
        let alias = target >= 0x200000;
        let mut vm = Vm::create(4 * 1024 * 1024)?;
        vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
        // A/D bits are already set, so the hardware walk does not stop the
        // run before its debug-register trap reaches the real RNG instruction.
        for (address, entry) in [(0x3000, 0x4027u64), (0x4000, 0x5027), (0x5000, 0xe7)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        vm.memory_mut()?[0x1000..0x2000].fill(0x90);
        vm.memory_mut()?[0x1000] = 0xe9;
        vm.memory_mut()?[0x1001..0x1005]
            .copy_from_slice(&((target as i64 - 0x1005) as i32).to_le_bytes());
        if alias {
            vm.memory_mut()?[0x5008..0x5010].copy_from_slice(&0xe7u64.to_le_bytes());
            vm.memory_mut()?[0x1100..0x110c].copy_from_slice(&[
                0x48, 0x0f, 0xc7, 0xf0, 0x48, 0x89, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
            ]);
        } else {
            for offset in [0x1100, 0x1120, 0x1140, 0x1160] {
                vm.memory_mut()?[offset..offset + 10]
                    .copy_from_slice(&[0x0f, 0xc7, 0xf0, 0x89, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
            }
        }
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        let mut random_exits = 0;
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            if exit.exit_reason == 57 {
                random_exits += 1;
                assert_eq!(exit.emulated_tsc, 1);
                vm.set_rdrand_value(0x1234)?;
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 3);
            assert_eq!(vm.get_regs()?.gprs.rbx, 0x1234);
            assert_eq!(random_exits, 1);
            break;
        }
    }
    println!("SVM_PAGE_RNG_BREAKPOINTS_PASS");
    Ok(())
}

fn test_page_fetch_rng() -> Result<(), Box<dyn std::error::Error>> {
    // Cover both a new instruction page and an instruction split across the
    // guard boundary. The latter must retain unrestricted scalar replay.
    for (target, branch) in [(0x2000usize, false), (0x1ffe, false), (0x2000, true)] {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
        for (address, entry) in [(0x3000, 0x4027u64), (0x4000, 0x5027), (0x5000, 0xe7)] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        vm.memory_mut()?[0x1000..0x2000].fill(0x90);
        vm.memory_mut()?[0x1000] = 0xe9;
        vm.memory_mut()?[0x1001..0x1005].copy_from_slice(&((target - 0x1005) as i32).to_le_bytes());
        let random = if branch { 0x2040 } else { target };
        if branch {
            vm.memory_mut()?[target..target + 5].copy_from_slice(&[0xe9, 0x3b, 0, 0, 0]);
        }
        vm.memory_mut()?[random..random + 12].copy_from_slice(&[
            0x48, 0x0f, 0xc7, 0xf0, // rdrand rax
            0x48, 0x89, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
        ]);
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.rip = 0x1000;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        let mut random_exits = 0;
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            if exit.exit_reason == 57 {
                assert_eq!(exit.emulated_tsc, if branch { 2 } else { 1 });
                random_exits += 1;
                vm.set_rdrand_value(0x1234)?;
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, if branch { 4 } else { 3 });
            assert_eq!(vm.get_regs()?.gprs.rbx, 0x1234);
            assert_eq!(random_exits, 1);
            break;
        }
    }
    println!("SVM_PAGE_FETCH_RNG_PASS");
    Ok(())
}

fn test_decoded_branch_exits() -> Result<(), Box<dyn std::error::Error>> {
    for looped in [false, true] {
        for target in [0x1080usize, 0x2000] {
            for taken in [false, true] {
                let mut results = Vec::new();
                for reference in [true, false] {
                    let mut vm = Vm::create(2 * 1024 * 1024)?;
                    vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
                    for (address, entry) in [(0x3000, 0x4027u64), (0x4000, 0x5027), (0x5000, 0xe7)]
                    {
                        vm.memory_mut()?[address..address + 8]
                            .copy_from_slice(&entry.to_le_bytes());
                    }
                    let mut code = if looped {
                        vec![
                            0xb9, 0x10, 0x27, 0, 0, 0xf3, 0x0f, 0x1e, 0xfa, 0xff, 0xc9, 0x75, 0xf8,
                            0x85, 0xc0, 0x0f, 0x84,
                        ]
                    } else {
                        vec![0x90, 0x90, 0x85, 0xc0, 0x0f, 0x84]
                    };
                    let before_test = if looped { 30_001 } else { 2 };
                    code.extend(((target - (0x1000 + code.len() + 4)) as i32).to_le_bytes());
                    code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]);
                    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                    vm.memory_mut()?[target..target + 9]
                        .copy_from_slice(&[0x48, 0x0f, 0xc7, 0xf0, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
                    for offset in [0x1800, 0x1820, 0x1840, 0x1860, 0x1880] {
                        vm.memory_mut()?[offset..offset + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
                    }
                    let mut regs = Regs::long_mode();
                    regs.control_regs.cr3 = Cr3::new(0x3000);
                    regs.rip = 0x1000;
                    regs.gprs.rsp = 0x8000;
                    regs.gprs.rax = if taken { 0 } else { 1 };
                    vm.set_regs(&regs)?;
                    if reference {
                        vm.set_single_step_range(0, 40_000)?;
                    }
                    let mut random_exits = 0;
                    loop {
                        let exit = vm.run()?;
                        match exit.exit_reason {
                            256 => continue,
                            57 => {
                                assert!(taken);
                                assert_eq!(exit.emulated_tsc, before_test + 2);
                                assert_eq!(vm.get_regs()?.rip, target as u64);
                                random_exits += 1;
                                vm.set_rdrand_value(0x1234)?;
                            }
                            258 => {
                                assert_eq!(exit.emulated_tsc, before_test + 3);
                                assert_eq!(random_exits, usize::from(taken));
                                let regs = vm.get_regs()?;
                                assert_eq!(regs.gprs.rax, 0);
                                assert_eq!(regs.gprs.rcx, 0);
                                results.push((
                                    exit.emulated_tsc,
                                    regs.rip,
                                    regs.rflags,
                                    regs.gprs.rax,
                                ));
                                break;
                            }
                            reason => {
                                return Err(
                                    format!("Unexpected branch-exit result: {reason}").into()
                                )
                            }
                        }
                    }
                    if !reference {
                        let exits = vm.get_exit_stats()?.total_exit_count();
                        assert!(exits < 100, "control-flow acceleration used {exits} exits");
                    }
                }
                assert_eq!(results[0], results[1]);
            }
        }
    }
    println!("SVM_DECODED_BRANCH_EXITS_PASS");
    Ok(())
}

fn test_data_translation_write() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    for (address, entry) in [
        (0x3000, 0x4027u64),
        (0x3008, 0x8027),
        (0x4000, 0x5027),
        (0x5000, 0xe7),
        (0x8000, 0x9027),
        (0x9000, 0xa027),
        (0xa000, 0xc027),
        (0xc000, 0x11111111),
        (0xd000, 0x22222222),
    ] {
        vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    vm.memory_mut()?[0x1000..0x100e].copy_from_slice(&[
        0x48, 0x8b, 0x1e, // mov rbx,[rsi]: prime the unrelated data translation
        0x48, 0x89, 0x07, // mov [rdi],rax: replace its PTE
        0x48, 0x8b, 0x16, // mov rdx,[rsi]: must use the replacement mapping
        0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ]);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x10000;
    regs.gprs.rsi = 1 << 39;
    regs.gprs.rdi = 0xa000;
    regs.gprs.rax = 0xd067;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 4);
        let r = vm.get_regs()?;
        assert_eq!(r.gprs.rbx, 0x11111111);
        assert_eq!(r.gprs.rdx, 0x22222222);
        break;
    }
    println!("SVM_DATA_TRANSLATION_WRITE_PASS");
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct LinuxCheckpoint {
    tsc: u64,
    memory_hash: Option<u64>,
    registers: [u64; 22],
}

fn linux_checkpoint(
    kernel: &str,
    initrd: &str,
    target: u64,
) -> Result<Vec<LinuxCheckpoint>, Box<dyn std::error::Error>> {
    linux_checkpoint_run(&mut linux_checkpoint_vm(kernel, initrd)?, 0, target)
}

fn linux_checkpoint_vm(kernel: &str, initrd: &str) -> Result<Vm, Box<dyn std::error::Error>> {
    let mut builder = VmBuilder::new().memory_mb(128).tsc_frequency(100_000_000);
    if let Ok(seed) = std::env::var("BEDROCK_CHECKPOINT_RNG_SEED") {
        let seed = seed.parse()?;
        builder = builder.rdrand(RdrandConfig::seeded_rng(seed));
        println!("SVM_LINUX_CHECKPOINT_CONFIG rng_seed={seed} tsc_frequency=100000000");
    }
    let mut vm = builder.build()?;
    let kernel = std::fs::read(kernel)?;
    let initrd = std::fs::read(initrd)?;
    let (entry, end) = load_kernel(vm.memory_mut()?, &kernel)?;
    vm.setup_linux_boot(
        &LinuxBootConfig::new(entry, end)
            .cmdline("console=ttyS0 nopti nokaslr mitigations=off audit=0")
            .initramfs(&initrd),
    )?;
    Ok(vm)
}

fn linux_fork_checkpoints(
    kernel: &str,
    initrd: &str,
    start: u64,
    target: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let replays = std::env::var("BEDROCK_CHECKPOINT_FORK_REPLAYS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(8);
    if start == 0 || start >= target || replays < 2 {
        return Err("Fork replay needs 0 < start < target and at least two replays".into());
    }
    let mut parent = linux_checkpoint_vm(kernel, initrd)?;
    let snapshot = linux_checkpoint_run(&mut parent, 0, start)?;
    let mut expected = None;
    for replay in 0..replays {
        let mut child = parent.fork()?;
        if replay == 0 && std::env::var_os("BEDROCK_CHECKPOINT_REFERENCE_STEP").is_some() {
            child.set_single_step_range(start, target)?;
            println!("SVM_LINUX_FORK_REFERENCE_STEP start={start} target={target}");
        }
        let checkpoints = linux_checkpoint_run(&mut child, start, target)?;
        if let Some(expected) = &expected {
            assert_eq!(
                expected, &checkpoints,
                "Fork checkpoint replay {replay} diverged"
            );
        } else {
            expected = Some(checkpoints);
        }
        println!("SVM_LINUX_FORK_CHECKPOINT_MATCH replay={replay} start={start} target={target}");
    }
    let mut hash = DefaultHasher::new();
    hash.write(parent.memory()?);
    assert_eq!(
        Some(hash.finish()),
        snapshot.last().unwrap().memory_hash,
        "Fork replay changed parent RAM"
    );
    println!("SVM_LINUX_FORK_CHECKPOINT_PASS replays={replays} start={start} target={target}");
    Ok(())
}

fn linux_checkpoint_run(
    vm: &mut Vm,
    start_tsc: u64,
    target: u64,
) -> Result<Vec<LinuxCheckpoint>, Box<dyn std::error::Error>> {
    let interval = (if start_tsc != 0 {
        std::env::var("BEDROCK_CHECKPOINT_FORK_INTERVAL")
            .or_else(|_| std::env::var("BEDROCK_CHECKPOINT_INTERVAL"))
    } else {
        std::env::var("BEDROCK_CHECKPOINT_INTERVAL")
    })
    .ok()
    .map(|value| value.parse::<u64>())
    .transpose()?
    .unwrap_or(target);
    if interval == 0 || target <= start_tsc {
        return Err("Checkpoint interval must be nonzero and target must exceed start".into());
    }
    let mut next = start_tsc.saturating_add(interval).min(target);
    let mut checkpoints = Vec::new();
    vm.set_stop_at_tsc(Some(next))?;
    let timeout = Duration::from_secs(
        std::env::var("BEDROCK_CHECKPOINT_TIMEOUT_SECONDS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(10),
    );
    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            return Err(format!("Linux checkpoint exceeded {timeout:?}").into());
        }
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259, "unexpected Linux guest exit");
        assert_eq!(exit.emulated_tsc, next);
        let seconds = start.elapsed().as_secs_f64();
        let stats = vm.get_exit_stats()?;
        println!(
            "{}",
            bedrock_vm::ExitStatsReport {
                stats: &stats,
                wall_clock: start.elapsed()
            }
        );
        let memory_hash = if vm.is_root() {
            let mut hash = DefaultHasher::new();
            hash.write(vm.memory()?);
            let value = hash.finish();
            println!("SVM_LINUX_CHECKPOINT_PASS tsc={next} seconds={seconds:.6} memory_hash={value:016x}");
            Some(value)
        } else {
            // Forks expose CoW RAM through the hypervisor rather than mmap.
            println!("SVM_LINUX_CHECKPOINT_REGISTERS tsc={next} seconds={seconds:.6}");
            None
        };
        let r = vm.get_regs()?;
        if vm.is_root() {
            println!(
                "SVM_GUEST_TABLES {:?}",
                svm_tables::page_table_count(vm.memory()?, r.control_regs.cr3.bits())
            );
        }
        let g = r.gprs;
        let registers = [
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
            r.control_regs.cr4.bits(),
        ];
        println!("REGISTERS {registers:x?}");
        checkpoints.push(LinuxCheckpoint {
            tsc: next,
            memory_hash,
            registers,
        });
        if next == target {
            return Ok(checkpoints);
        }
        next = next.saturating_add(interval).min(target);
        vm.set_stop_at_tsc(Some(next))?;
    }
}
