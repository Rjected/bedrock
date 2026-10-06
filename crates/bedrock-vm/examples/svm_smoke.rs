// SPDX-License-Identifier: GPL-2.0
//! Hardware smoke test: execute a tiny real-mode guest without a Linux image.
//! Run inside the Rust-enabled Bedrock test host, not directly on Linux 6.8.
use bedrock_vm::{RdrandConfig, Regs, SegmentRegister, Vm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    // mov bx,0x1234; inc bx; xor eax,eax; vmmcall (shutdown).
    let code = [0xbb, 0x34, 0x12, 0x43, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9];
    vm.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(2))?;
    let mut stopped = false;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        if exit.exit_reason == 259 {
            assert!(!stopped);
            assert_eq!(exit.emulated_tsc, 2);
            assert_eq!(vm.get_regs()?.rip, 0x1004);
            stopped = true;
            vm.set_stop_at_tsc(None)?;
            for _ in 0..2 {
                let snapshot = vm.fork()?;
                loop {
                    let resumed = snapshot.run()?;
                    if resumed.exit_reason == 256 {
                        continue;
                    }
                    assert_eq!(resumed.exit_reason, 258);
                    assert_eq!(resumed.emulated_tsc, 3);
                    assert_eq!(snapshot.get_regs()?.gprs.rbx & 0xffff, 0x1235);
                    break;
                }
            }
            continue;
        }
        println!(
            "SVM exit: {} at virtual TSC {}",
            exit.reason_str(),
            exit.emulated_tsc
        );
        assert_eq!(exit.exit_reason, 258, "guest did not shut down");
        let regs = vm.get_regs()?;
        assert_eq!(regs.gprs.rbx & 0xffff, 0x1235);
        assert_eq!(regs.rip, 0x100a);
        assert_eq!(regs.rflags & (1 << 8), 0, "hypervisor TF leaked");
        assert_eq!(exit.emulated_tsc, 3);
        break;
    }
    assert!(stopped, "missed the exact instruction deadline");
    // Two children write the same shared page. Each must see its own value,
    // and the parent's page must remain zero.
    let mut parent = Vm::create(2 * 1024 * 1024)?;
    let code = [
        0xa3, 0x00, 0x20, 0x8b, 0x1e, 0x00, 0x20, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ];
    parent.memory_mut()?[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    parent.set_regs(&regs)?;
    for value in [0x1111, 0x2222, 0x1111] {
        let child = parent.fork()?;
        let mut child_regs = regs.clone();
        child_regs.gprs.rax = value;
        child.set_regs(&child_regs)?;
        loop {
            let exit = child.run()?;
            if exit.exit_reason == 256 || exit.exit_reason == 262 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(child.get_regs()?.gprs.rbx & 0xffff, value);
            assert_eq!(exit.emulated_tsc, 3);
            break;
        }
    }
    assert_eq!(&parent.memory()?[0x2000..0x2002], &[0, 0]);
    // A guest PUSHF must not expose the TF used by the SVM step backend.
    let mut flags_vm = Vm::create(2 * 1024 * 1024)?;
    let flags_code = [0x9c, 0x5b, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9];
    flags_vm.memory_mut()?[0x1000..0x1008].copy_from_slice(&flags_code);
    flags_vm.set_regs(&regs)?;
    loop {
        let exit = flags_vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(flags_vm.get_regs()?.gprs.rbx & 0xffff, 2);
        assert_eq!(exit.emulated_tsc, 3);
        break;
    }
    let mut restore_vm = Vm::create(2 * 1024 * 1024)?;
    let restore_code = [0x9c, 0x9d, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9];
    restore_vm.memory_mut()?[0x1000..0x1008].copy_from_slice(&restore_code);
    restore_vm.set_regs(&regs)?;
    loop {
        let exit = restore_vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 3);
        assert_eq!(restore_vm.get_regs()?.rflags & (1 << 8), 0);
        break;
    }
    // AMD has no RNG intercept. Both instructions must take the software
    // path and request the exact values supplied by userspace.
    let mut random_vm = Vm::create(2 * 1024 * 1024)?;
    let random_code = [
        0x0f, 0xc7, 0xf3, 0x0f, 0xc7, 0xfa, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ];
    random_vm.memory_mut()?[0x1000..0x100c].copy_from_slice(&random_code);
    random_vm.set_regs(&regs)?;
    random_vm.set_rdrand_config(&RdrandConfig::exit_to_userspace())?;
    let mut requests = 0;
    loop {
        let exit = random_vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        if exit.exit_reason == 57 {
            let value = [0x1357, 0x2468][requests];
            random_vm.set_rdrand_value(value)?;
            requests += 1;
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(requests, 2);
        let registers = random_vm.get_regs()?;
        assert_eq!(registers.gprs.rbx & 0xffff, 0x1357);
        assert_eq!(registers.gprs.rdx & 0xffff, 0x2468);
        break;
    }
    println!("SVM_SMOKE_PASS: execution, exact deadline, fork COW and replay");
    Ok(())
}
