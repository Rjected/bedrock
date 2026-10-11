// SPDX-License-Identifier: GPL-2.0

//! Adapter for the existing Bedrock VMCS field API. Guest state lives in the
//! hardware VMCB. VMX-only bookkeeping lives in its unused software area.
//! Keeping this adapter inside the VMCB page makes existing fork copies work.

use super::vmcb::{offset as o, segment_attributes_from_vmx, segment_attributes_to_vmx, Vmcb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedField(pub u32);

fn segment(index: u32) -> usize {
    [o::ES, o::CS, o::SS, o::DS, o::FS, o::GS, o::LDTR, o::TR][index as usize]
}

fn location(field: u32) -> Result<(usize, usize), UnsupportedField> {
    Ok(match field {
        0x0800..=0x080e if field & 1 == 0 => (segment((field - 0x0800) / 2), 2),
        0x4800..=0x480e if field & 1 == 0 => (segment((field - 0x4800) / 2) + 4, 4),
        0x4814..=0x4822 if field & 1 == 0 => (segment((field - 0x4814) / 2) + 2, 2),
        0x6806..=0x6814 if field & 1 == 0 => (segment((field - 0x6806) / 2) + 8, 8),
        0x4810 => (o::GDTR + 4, 4),
        0x4812 => (o::IDTR + 4, 4),
        0x6816 => (o::GDTR + 8, 8),
        0x6818 => (o::IDTR + 8, 8),
        0x6800 => (o::CR0, 8),
        0x6802 => (o::CR3, 8),
        0x6804 => (o::CR4, 8),
        0x681a => (o::DR7, 8),
        0x681c => (o::RSP, 8),
        0x681e => (o::RIP, 8),
        0x6820 => (o::RFLAGS, 8),
        0x2802 => (o::DEBUGCTL, 8),
        0x2804 => (o::PAT, 8),
        0x2806 => (o::EFER, 8),
        0x482a => (o::SYSENTER_CS, 8),
        0x6824 => (o::SYSENTER_ESP, 8),
        0x6826 => (o::SYSENTER_EIP, 8),
        0x0000 => (o::ASID, 4),
        0x201a => (o::NESTED_CR3, 8),
        0x4824 => (o::INT_SHADOW, 4),
        // Saved common controls and normalized exit information.
        0x4000..=0x401e if field & 1 == 0 => (0xc00 + ((field - 0x4000) / 2) as usize * 8, 4),
        0x4400..=0x440e if field & 1 == 0 => (0xd00 + ((field - 0x4400) / 2) as usize * 8, 4),
        0x6000..=0x6006 if field & 1 == 0 => (0xd80 + ((field - 0x6000) / 2) as usize * 8, 8),
        0x2004..=0x200a if field & 1 == 0 => (0xda0 + ((field - 0x2004) / 2) as usize * 8, 8),
        0x2808 => (0xdc0, 8),
        0x2400 => (0xdc8, 8),
        0x6400 => (0xdd0, 8),
        0x640a => (0xdd8, 8),
        0x6822 => (0xde0, 8),
        0x4826 => (0xde8, 4),
        0x482e => (0xdf0, 4),
        0x2800 => (0xdf8, 8),
        _ => return Err(UnsupportedField(field)),
    })
}

fn host_field(field: u32) -> bool {
    field & 0xc00 == 0xc00
}

pub fn read(v: &Vmcb, field: u32) -> Result<u64, UnsupportedField> {
    // Host state is saved/restored by VMRUN and the VMLOAD/VMSAVE wrapper.
    if host_field(field) {
        return Ok(0);
    }
    // The global-batch planner reads these guest fields on nearly every
    // entry. Resolve them directly before the general VMCS-to-VMCB mapping.
    match field {
        0x4816 => return Ok(segment_attributes_to_vmx(v.read(o::CS + 2, 2) as u16) as u64),
        0x6802 => return Ok(v.read(o::CR3, 8)),
        0x681a => return Ok(v.read(o::DR7, 8)),
        0x681e => return Ok(v.read(o::RIP, 8)),
        0x6820 => return Ok(v.read(o::RFLAGS, 8)),
        _ => {}
    }
    let (offset, width) = location(field)?;
    let raw = v.read(offset, width);
    Ok(match field {
        0x4814..=0x4822 => segment_attributes_to_vmx(raw as u16) as u64,
        0x2806 => raw & !(1 << 12), // SVME is a host implementation detail
        _ => raw,
    })
}

pub fn write(v: &mut Vmcb, field: u32, value: u64) -> Result<(), UnsupportedField> {
    if host_field(field) {
        return Ok(());
    }
    let (offset, width) = location(field)?;
    let raw = match field {
        0x4814..=0x4822 => segment_attributes_from_vmx(value as u32) as u64,
        0x2806 => value | (1 << 12),
        _ => value,
    };
    v.write(offset, width, raw);
    match field {
        0x0802 => v.write(o::CPL, 1, value & 3),
        0x4004 => v.write(o::INTERCEPT_EXCEPTIONS, 4, u32::MAX as u64),
        0x4016 => v.write(o::EVENT_INJECTION, 4, value),
        0x4018 => v.write(o::EVENT_INJECTION + 4, 4, value),
        0x4002 => {
            // Translate Intel interrupt-window exiting to SVM's virtual IRQ.
            let window = value & (1 << 2) != 0;
            v.intercept(100, window);
            let mut ctl = v.read(o::INT_CONTROL, 4) & !((1 << 8) | (15 << 16));
            if window {
                ctl |= (1 << 8) | (15 << 16);
            }
            v.write(o::INT_CONTROL, 4, ctl);
        }
        _ => {}
    }
    Ok(())
}

pub fn record_exit(v: &mut Vmcb, e: &super::exits::Exit) {
    v.record_exit(e);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn common_guest_registers_land_in_hardware_state() {
        let mut v = Vmcb::new();
        write(&mut v, 0x6802, 0x123000).unwrap();
        write(&mut v, 0x0802, 0x33).unwrap();
        write(&mut v, 0x080e, 0x28).unwrap();
        write(&mut v, 0x6814, 0xffff800000000000).unwrap();
        write(&mut v, 0x4816, 0xa0fb).unwrap();
        assert_eq!(v.read(o::CR3, 8), 0x123000);
        assert_eq!(v.read(o::TR, 2), 0x28);
        assert_eq!(v.read(o::TR + 8, 8), 0xffff800000000000);
        assert_eq!(v.read(o::CS + 2, 2), 0xafb);
        assert_eq!(v.read(o::CPL, 1), 3);
        assert_eq!(read(&v, 0x4816), Ok(0xa0fb));
    }
    #[test]
    fn batch_planner_fields_read_hardware_state() {
        let mut v = Vmcb::new();
        for (offset, field, value) in [
            (o::CR3, 0x6802, 0x1230_0000),
            (o::DR7, 0x681a, 0x400),
            (o::RIP, 0x681e, 0xffff_8000_1234_5678),
            (o::RFLAGS, 0x6820, 0x202),
        ] {
            v.write(offset, 8, value);
            assert_eq!(read(&v, field), Ok(value));
        }
    }
    #[test]
    fn control_and_exit_state_do_not_overlap() {
        let mut v = Vmcb::new();
        v.initialize();
        write(&mut v, 0x4002, 4).unwrap();
        write(&mut v, 0x4402, 48).unwrap();
        write(&mut v, 0x6400, 3).unwrap();
        write(&mut v, 0x2400, 0x400000).unwrap();
        assert_eq!(read(&v, 0x4002), Ok(4));
        assert_eq!(read(&v, 0x4402), Ok(48));
        assert_eq!(read(&v, 0x6400), Ok(3));
        assert_eq!(read(&v, 0x2400), Ok(0x400000));
        assert_ne!(v.read(o::INTERCEPT_MISC1, 4) & (1 << 4), 0);
        assert_ne!(v.read(o::INT_CONTROL, 4) & (1 << 24), 0);
        write(&mut v, 0x4002, 0).unwrap();
        assert_eq!(v.read(o::INT_CONTROL, 4) & (1 << 8), 0);
    }
}
