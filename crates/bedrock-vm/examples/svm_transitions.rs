// SPDX-License-Identifier: GPL-2.0
//! Hardware regression for SYSCALL/SYSRET while SVM single-steps the guest.
use bedrock_vm::{Cr3, Efer, Gdtr, Idtr, Regs, SegmentRegister, Vm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("SVM_CASE").as_deref() == Ok("page-fault") {
        return test_page_fault();
    }
    if std::env::var("SVM_CASE").as_deref() == Ok("mov-ss") {
        return test_mov_ss_deadline();
    }
    test_mov_ss_deadline()?;
    test_syscall(false)?;
    test_syscall(true)?;
    test_interrupt()?;
    test_iret()?;
    test_page_fault()?;
    test_guest_debug_trap()?;
    test_guest_breakpoint_trap()?;
    Ok(())
}

fn test_mov_ss_deadline() -> Result<(), Box<dyn std::error::Error>> {
    for instruction in [
        &[0x8e, 0xd0][..],             // AX
        &[0x41, 0x8e, 0xd0][..],       // R8W
        &[0x41, 0x66, 0x8e, 0xd0][..], // Legacy prefix cancels REX.
        &[0x8e, 0xd4][..],             // SP comes from the VMCB, not the GPR save area.
    ] {
        test_mov_ss_register_deadline(instruction)?;
    }
    println!("SVM_MOV_SS_DEADLINE_PASS");
    Ok(())
}

fn test_mov_ss_register_deadline(instruction: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    memory[0x9018..0x9020].copy_from_slice(&0x00cf93000000ffffu64.to_le_bytes());
    let next_rip = 0x1000 + instruction.len();
    memory[0x1000..next_rip].copy_from_slice(instruction);
    memory[next_rip..next_rip + 6].copy_from_slice(&[0x90, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.descriptor_tables.gdtr = Gdtr::new(0x9000, 0x1f);
    regs.rip = 0x1000;
    regs.gprs.rax = 0x18;
    regs.gprs.r8 = 0x18;
    regs.gprs.rsp = if instruction == [0x8e, 0xd4] {
        0x18
    } else {
        0x8000
    };
    vm.set_regs(&regs)?;
    vm.set_stop_at_tsc(Some(1))?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 259);
        let stopped = vm.get_regs()?;
        assert_eq!(exit.emulated_tsc, 1);
        assert_eq!(
            stopped.rip, next_rip as u64,
            "MOV SS deadline crossed the following instruction"
        );
        assert_eq!(stopped.segment_regs.ss.selector.bits(), 0x18);
        vm.set_stop_at_tsc(Some(2))?;
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 259);
            assert_eq!(exit.emulated_tsc, 2);
            assert_eq!(vm.get_regs()?.rip, next_rip as u64 + 1);
            break;
        }
        break;
    }
    Ok(())
}

fn test_syscall(prefixed: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    // Identity-map 2MB with four-level guest paging and a huge-page PDE.
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    let mut code = Vec::new();
    let mut write_msr = |index: u32, value: u64| {
        code.push(0xb9);
        code.extend(index.to_le_bytes()); // mov ecx
        code.push(0xb8);
        code.extend((value as u32).to_le_bytes());
        code.push(0xba);
        code.extend(((value >> 32) as u32).to_le_bytes());
        code.extend([0x0f, 0x30]); // wrmsr
    };
    write_msr(0xc0000081, (0x10 << 32) | (0x20u64 << 48)); // STAR
    write_msr(0xc0000082, 0x2000); // LSTAR
    write_msr(0xc0000084, 0x100); // FMASK clears TF on SYSCALL
    if prefixed {
        code.extend([0x66, 0x48, 0x2e]); // Legacy prefix after REX cancels REX.
    }
    code.extend([0x0f, 0x05]); // syscall
    code.extend([0x31, 0xc0, 0x0f, 0x01, 0xd9]); // xor eax; shutdown
    memory[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    memory[0x2000..0x200a].fill(0x90); // ten retired NOPs inside the handler
    let sysret: &[u8] = if prefixed {
        &[0x66, 0x48, 0x40, 0x48, 0x0f, 0x07]
    } else {
        &[0x48, 0x0f, 0x07]
    };
    memory[0x200a..0x200a + sysret.len()].copy_from_slice(sysret);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.extended_control.efer = Efer::new(regs.extended_control.efer.bits() | 1);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    let trace = std::env::var_os("SVM_TRACE_STEPS").is_some();
    let mut deadline = 1;
    if trace {
        vm.set_stop_at_tsc(Some(deadline))?;
    }
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        if exit.exit_reason == 259 && trace {
            let registers = vm.get_regs()?;
            println!(
                "step {} rip={:#x} flags={:#x} r11={:#x}",
                exit.emulated_tsc, registers.rip, registers.rflags, registers.gprs.r11
            );
            deadline += 1;
            vm.set_stop_at_tsc(Some(deadline))?;
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(
            exit.emulated_tsc, 25,
            "lost instructions at a privilege transition"
        );
        let registers = vm.get_regs()?;
        assert_eq!(
            registers.gprs.r11 & (1 << 8),
            0,
            "SYSCALL saved hypervisor TF into R11"
        );
        assert_eq!(registers.segment_regs.cs.selector.bits(), 0x33);
        assert_eq!(registers.rflags & (1 << 8), 0);
        println!(
            "{}",
            if prefixed {
                "SVM_PREFIXED_TRANSITIONS_PASS"
            } else {
                "SVM_TRANSITIONS_PASS"
            }
        );
        break;
    }
    Ok(())
}

fn test_interrupt() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    memory[12..16].copy_from_slice(&[0x00, 0x20, 0x00, 0x00]); // INT3 IVT entry
    memory[0x1000..0x1007].copy_from_slice(&[0xcc, 0x66, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
    memory[0x2000..0x200a].fill(0x90);
    memory[0x200a..0x200d].copy_from_slice(&[0x9c, 0x5b, 0xcf]);
    let mut regs = Regs::real_mode();
    regs.segment_regs.cs = SegmentRegister::new(0, 0x9b, 0xffff, 0);
    regs.descriptor_tables.idtr = Idtr::new(0, 0x3ff);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(
            exit.emulated_tsc, 15,
            "lost instructions inside an interrupt handler"
        );
        assert_eq!(
            u16::from_le_bytes(vm.memory()?[0x7ffe..0x8000].try_into()?),
            2,
            "interrupt frame exposed hypervisor TF"
        );
        println!("SVM_INTERRUPT_PASS");
        break;
    }
    Ok(())
}

fn test_iret() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    for (address, entry) in [
        (0x3000, 0x4007u64),
        (0x4000, 0x5007),
        (0x5000, 0x87),
        (0x6008, 0x00af9b000000ffff),
        (0x6010, 0x00cf93000000ffff),
    ] {
        memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    memory[0x1000..0x1002].copy_from_slice(&[0x48, 0xcf]);
    memory[0x2000..0x200a].fill(0x90);
    memory[0x200a..0x200f].copy_from_slice(&[0x31, 0xc0, 0x0f, 0x01, 0xd9]);
    for (index, word) in [0x2000u64, 8, 2, 0x9000, 0x10].iter().enumerate() {
        memory[0x8000 + index * 8..0x8008 + index * 8].copy_from_slice(&word.to_le_bytes());
    }
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.descriptor_tables.gdtr = Gdtr::new(0x6000, 0x17);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(exit.emulated_tsc, 12, "lost instructions after IRET");
        assert_eq!(vm.get_regs()?.gprs.rsp, 0x9000);
        println!("SVM_IRET_PASS");
        break;
    }
    Ok(())
}

fn test_page_fault() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    for (address, entry) in [
        (0x3000, 0x4007u64),
        (0x4000, 0x5007),
        (0x5000, 0x87),
        (0x6008, 0x00af9b000000ffff),
        (0x6010, 0x00cf93000000ffff),
    ] {
        memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    // Read immediately beyond the mapped huge page, then shut down after recovery.
    memory[0x1000..0x100f].copy_from_slice(&[
        0x48, 0xa1, 0, 0, 0x20, 0, 0, 0, 0, 0, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ]);
    let handler = [
        0x0f, 0x20, 0xd0, // mov rax, cr2
        0x48, 0xa3, 0, 0x90, 0, 0, 0, 0, 0, 0,    // save fault address
        0x58, // discard page-fault error code
        0x48, 0x83, 0x04, 0x24, 10, // skip faulting instruction in saved RIP
        0x48, 0xcf, // iretq
    ];
    memory[0x2000..0x2000 + handler.len()].copy_from_slice(&handler);
    memory[0x70e0..0x70f0].copy_from_slice(&[0, 0x20, 8, 0, 0, 0x8e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.segment_regs.cs = SegmentRegister::new(8, 0xa09b, u32::MAX, 0);
    regs.segment_regs.ss = SegmentRegister::new(0x10, 0xc093, u32::MAX, 0);
    regs.descriptor_tables.gdtr = Gdtr::new(0x6000, 0x17);
    regs.descriptor_tables.idtr = Idtr::new(0x7000, 0xfff);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(
            u64::from_le_bytes(vm.memory()?[0x9000..0x9008].try_into()?),
            0x200000
        );
        let flags = u64::from_le_bytes(vm.memory()?[0x7fe8..0x7ff0].try_into()?);
        assert_eq!(
            flags & (1 << 8),
            0,
            "page-fault frame exposed hypervisor TF"
        );
        assert_ne!(flags & (1 << 16), 0, "page-fault frame must set RF");
        assert_eq!(vm.get_regs()?.gprs.rsp, 0x8000);
        assert_eq!(
            exit.emulated_tsc, 6,
            "lost instructions in page-fault handler"
        );
        println!("SVM_PAGE_FAULT_PASS");
        break;
    }
    Ok(())
}

fn test_guest_debug_trap() -> Result<(), Box<dyn std::error::Error>> {
    let mut vm = Vm::create(2 * 1024 * 1024)?;
    let memory = vm.memory_mut()?;
    for (address, entry) in [(0x3000, 0x4007u64), (0x4000, 0x5007), (0x5000, 0x87)] {
        memory[address..address + 8].copy_from_slice(&entry.to_le_bytes());
    }
    memory[0x1000..0x1010].copy_from_slice(&[
        0x90, 0x68, 2, 1, 0, 0, 0x9d, 0x90, // Enable guest TF, then NOP.
        0x48, 0xff, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9,
    ]);
    let mut regs = Regs::long_mode();
    regs.control_regs.cr3 = Cr3::new(0x3000);
    regs.rip = 0x1000;
    regs.gprs.rsp = 0x8000;
    vm.set_regs(&regs)?;
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        // Guest #DB follows retirement. The existing exception policy returns
        // it to userspace; it must not replay the subsequent INC instruction.
        assert_eq!(exit.exit_reason, 0);
        let r = vm.get_regs()?;
        assert_eq!(r.rip, 0x1008);
        assert_eq!(r.gprs.rbx, 0);
        assert_eq!(r.gprs.rsp, 0x8000);
        assert_ne!(r.rflags & (1 << 8), 0);
        let mut resumed = r;
        resumed.rflags &= !(1 << 8);
        vm.set_regs(&resumed)?;
        break;
    }
    loop {
        let exit = vm.run()?;
        if exit.exit_reason == 256 {
            continue;
        }
        assert_eq!(exit.exit_reason, 258);
        assert_eq!(
            exit.emulated_tsc, 6,
            "lost the instruction preceding guest #DB"
        );
        assert_eq!(vm.get_regs()?.gprs.rbx, 1);
        break;
    }
    println!("SVM_GUEST_DEBUG_TRAP_PASS");
    Ok(())
}

fn test_guest_breakpoint_trap() -> Result<(), Box<dyn std::error::Error>> {
    for guarded in [false, true] {
        let mut vm = Vm::create(2 * 1024 * 1024)?;
        for (address, entry) in [
            (0x3000, 0x4027u64),
            (0x4000, 0x5027),
            (0x5000, 0xe7),
            (0x6008, 0x00af9b000000ffff),
            (0x6010, 0x00cf93000000ffff),
        ] {
            vm.memory_mut()?[address..address + 8].copy_from_slice(&entry.to_le_bytes());
        }
        vm.memory_mut()?[0x1000..0x1009]
            .copy_from_slice(&[0xcc, 0x48, 0xff, 0xc3, 0x31, 0xc0, 0x0f, 0x01, 0xd9]);
        vm.memory_mut()?[0x2000..0x2009].copy_from_slice(&[
            0x48, 0x8b, 0x34, 0x24, // mov rsi,[rsp]: saved return RIP
            0x49, 0xff, 0xc0, // inc r8
            0x48, 0xcf, // iretq
        ]);
        vm.memory_mut()?[0x7030..0x7040]
            .copy_from_slice(&[0, 0x20, 8, 0, 0, 0x8e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        if guarded {
            for offset in [0x1800, 0x1820, 0x1840, 0x1860, 0x1880] {
                vm.memory_mut()?[offset..offset + 3].copy_from_slice(&[0x0f, 0xc7, 0xf0]);
            }
        }
        let mut regs = Regs::long_mode();
        regs.control_regs.cr3 = Cr3::new(0x3000);
        regs.segment_regs.cs = SegmentRegister::new(8, 0xa09b, u32::MAX, 0);
        regs.segment_regs.ss = SegmentRegister::new(0x10, 0xc093, u32::MAX, 0);
        regs.descriptor_tables.gdtr = Gdtr::new(0x6000, 0x17);
        regs.descriptor_tables.idtr = Idtr::new(0x7000, 0xfff);
        regs.rip = 0x1000;
        regs.gprs.rsp = 0x8000;
        vm.set_regs(&regs)?;
        loop {
            let exit = vm.run()?;
            if exit.exit_reason == 256 {
                continue;
            }
            assert_eq!(exit.exit_reason, 258);
            assert_eq!(exit.emulated_tsc, 6);
            let r = vm.get_regs()?;
            assert_eq!(r.gprs.rbx, 1);
            assert_eq!(r.gprs.r8, 1);
            assert_eq!(r.gprs.rsi, 0x1001);
            assert_eq!(r.gprs.rsp, 0x8000);
            break;
        }
    }
    println!("SVM_GUEST_BREAKPOINT_TRAP_PASS");
    Ok(())
}
