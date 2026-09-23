// SPDX-License-Identifier: GPL-2.0

//! Linux boot configuration and setup.

use crate::error::VmError;
use crate::vm::Vm;

use super::{
    linux_boot_regs, setup_boot_params, setup_gdt, setup_mptable, setup_page_tables, write_cmdline,
};

/// Configuration for [`Vm::setup_linux_boot`]. The kernel must already be
/// loaded into guest memory.
///
/// # Example
///
/// ```ignore
/// use bedrock_vm::{VmBuilder, LinuxBootConfig};
///
/// let mut vm = VmBuilder::new()
///     .memory_mb(64)
///     .build()?;
///
/// // Load kernel into memory first (ELF parsing is caller's responsibility)
/// let (kernel_entry, kernel_end) = bedrock_vm::load_kernel(vm.memory_mut()?, &kernel_data)?;
///
/// // Configure and setup Linux boot
/// let config = LinuxBootConfig::new(kernel_entry, kernel_end)
///     .cmdline("console=ttyS0")
///     .initramfs(&initramfs_data);
///
/// let info = vm.setup_linux_boot(&config)?;
/// println!("Initramfs loaded at {:#x}", info.initramfs_addr.unwrap());
/// ```
#[derive(Debug, Clone)]
pub struct LinuxBootConfig<'a> {
    pub kernel_entry: u64,
    /// Highest address used by the kernel (for initramfs placement).
    pub kernel_end: usize,
    /// Placed after the kernel, 2MB-aligned.
    pub initramfs: Option<&'a [u8]>,
    pub cmdline: &'a str,
}

impl<'a> LinuxBootConfig<'a> {
    pub fn new(kernel_entry: u64, kernel_end: usize) -> Self {
        Self {
            kernel_entry,
            kernel_end,
            initramfs: None,
            cmdline: "",
        }
    }

    pub fn cmdline(mut self, cmdline: &'a str) -> Self {
        self.cmdline = cmdline;
        self
    }

    pub fn initramfs(mut self, data: &'a [u8]) -> Self {
        self.initramfs = Some(data);
        self
    }
}

/// What [`Vm::setup_linux_boot`] configured.
#[derive(Debug, Clone)]
pub struct LinuxBootInfo {
    pub gdt_base: u64,
    pub gdt_limit: u16,
    pub initramfs_addr: Option<u64>,
    pub initramfs_size: Option<usize>,
}

const ALIGN_2MB: usize = 0x1FFFFF;

impl Vm {
    /// Write the 64-bit GDT, identity page tables, MP tables, boot_params,
    /// cmdline and optional initramfs into guest memory and set the initial
    /// registers. Root VMs only; the kernel must already be loaded.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use bedrock_vm::{VmBuilder, LinuxBootConfig};
    ///
    /// let mut vm = VmBuilder::new().memory_mb(64).build()?;
    ///
    /// // Load kernel first
    /// let (entry, end) = bedrock_vm::load_kernel(vm.memory_mut()?, &kernel_elf)?;
    ///
    /// // Setup Linux boot
    /// let config = LinuxBootConfig::new(entry, end)
    ///     .cmdline("console=ttyS0 quiet");
    /// vm.setup_linux_boot(&config)?;
    ///
    /// // VM is ready to run
    /// loop {
    ///     let exit = vm.run()?;
    ///     // ...
    /// }
    /// ```
    pub fn setup_linux_boot(&mut self, config: &LinuxBootConfig) -> Result<LinuxBootInfo, VmError> {
        if !self.is_root() {
            return Err(VmError::InvalidConfiguration {
                reason: "cannot setup Linux boot on forked VM (no direct memory access)"
                    .to_string(),
            });
        }

        let memory_size = self.memory_size();

        let memory = self
            .memory_mut()
            .map_err(|e| VmError::InvalidConfiguration {
                reason: format!("failed to access guest memory: {}", e),
            })?;

        let (gdt_base, gdt_limit) = setup_gdt(memory);

        setup_page_tables(memory, memory_size);

        setup_mptable(memory);

        let (initramfs_addr, initramfs_size) = if let Some(data) = config.initramfs {
            let addr = (config.kernel_end + ALIGN_2MB) & !ALIGN_2MB;
            let size = data.len();
            let end = addr + size;

            if end > memory_size {
                return Err(VmError::InvalidConfiguration {
                    reason: format!(
                        "initramfs too large: {} bytes would exceed guest memory (end {:#x} > {:#x})",
                        size, end, memory_size
                    ),
                });
            }

            memory[addr..end].copy_from_slice(data);
            (Some(addr as u64), Some(size))
        } else {
            (None, None)
        };

        setup_boot_params(
            memory,
            memory_size,
            config.cmdline,
            initramfs_addr,
            initramfs_size,
        );
        write_cmdline(memory, config.cmdline);

        let regs = linux_boot_regs(config.kernel_entry, gdt_base, gdt_limit);
        self.set_regs(&regs).map_err(|e| VmError::Ioctl {
            operation: "SET_REGS",
            source: e,
        })?;

        Ok(LinuxBootInfo {
            gdt_base,
            gdt_limit,
            initramfs_addr,
            initramfs_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_boot_config_builder() {
        let config = LinuxBootConfig::new(0x1000000, 0x2000000)
            .cmdline("console=ttyS0")
            .initramfs(&[1, 2, 3]);

        assert_eq!(config.kernel_entry, 0x1000000);
        assert_eq!(config.kernel_end, 0x2000000);
        assert_eq!(config.cmdline, "console=ttyS0");
        assert!(config.initramfs.is_some());
    }

    #[test]
    fn test_linux_boot_config_defaults() {
        let config = LinuxBootConfig::new(0x1000000, 0x2000000);

        assert_eq!(config.kernel_entry, 0x1000000);
        assert_eq!(config.kernel_end, 0x2000000);
        assert_eq!(config.cmdline, "");
        assert!(config.initramfs.is_none());
    }
}
