#[cfg(not(feature = "cargo"))]
use super::super::prelude::*;
#[cfg(feature = "cargo")]
use crate::prelude::*;

use super::instruction_counter::InstructionCounterError;

/// Error returned during VMX initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxInitError {
    /// VMX is not supported on this CPU.
    Unsupported,
    /// Failed to read VMX basic info MSR.
    FailedToReadBasicInfo(MsrError),
    /// Failed to enable VMX operation.
    FailedToEnableCPU { core: usize, error: VmxCpuInitError },
    /// Failed to allocate memory.
    MemoryAllocationFailed,
}

/// Error returned during VMX feature control configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxConfigureFeatureControlError {
    /// The IA32_FEATURE_CONTROL MSR is locked and VMX is not enabled.
    Locked,
    MsrReadFailed(MsrError),
    MsrWriteFailed(MsrError),
}

/// VMXON failure (SDM Vol 3C §32.2, "VMXON").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxonError {
    /// VMfailInvalid (CF=1): pointer unaligned, beyond physical-address width,
    /// or revision ID mismatch / bit 31 set.
    InvalidPointer,

    /// VMfailValid error 15 (ZF=1): already in VMX root operation.
    AlreadyInVmxOperation,
}

/// VMXOFF failure (SDM Vol 3C §32.2, "VMXOFF").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxoffError {
    /// VMfailValid error 23 (ZF=1): dual-monitor treatment of SMIs and SMM active.
    DualMonitorTreatmentActive,
}

/// INVEPT failure (SDM Vol 3C, "INVEPT").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InveptError {
    /// VMfailInvalid (CF=1): not in VMX operation or invalid type operand.
    InvalidOperand,
    /// VMfailValid (ZF=1): INVEPT type not supported.
    NotSupported,
}

/// INVVPID failure (SDM Vol 3C, "INVVPID").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvvpidError {
    /// VMfailInvalid (CF=1): not in VMX operation or invalid type operand.
    InvalidOperand,
    /// VMfailValid (ZF=1): INVVPID type not supported.
    NotSupported,
}

/// Error returned during VMXON region allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxonAllocError {
    /// Failed to allocate memory for the VMXON region.
    MemoryAllocationFailed,
    /// VMXON instruction failed.
    VmxonFailed(VmxonError),
}

/// Error returned during VmxCpu initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxCpuInitError {
    /// Failed to configure feature control MSR.
    FeatureControlConfigFailed(VmxConfigureFeatureControlError),
    /// Failed to enable VMX via CR4.
    FailedToEnableVMX(CrError),
    /// Failed to allocate VMXON region.
    VmxonAllocFailed(VmxonAllocError),
}

/// VMREAD failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsReadError {
    /// VMfailInvalid (CF=1): no current VMCS.
    VmcsNotLoaded,
    /// VMfailValid error 12 (ZF=1): unsupported field encoding.
    InvalidField,
}

pub type VmcsReadResult<T> = Result<T, VmcsReadError>;

/// VMWRITE failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsWriteError {
    /// VMfailInvalid (CF=1): no current VMCS.
    VmcsNotLoaded,
    /// VMfailValid error 12 (ZF=1): unsupported field encoding.
    InvalidField,
    /// VMfailValid error 13 (ZF=1): read-only field (e.g. VM-exit info, unless
    /// IA32_VMX_MISC allows writes).
    ReadOnlyField,
}

pub type VmcsWriteResult = Result<(), VmcsWriteError>;

/// Error returned during VMCS creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsAllocError {
    /// Failed to allocate memory for the VMCS.
    MemoryAllocationFailed,
}

/// Error returned during VMCS setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsSetupError {
    Clear(&'static str),
    Guard(&'static str),
    HostState(VmcsWriteError),
    Controls(VmcsWriteError),
    EptPointer(VmcsWriteError),
}

/// Error type for memory access operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    /// Address out of range.
    OutOfRange,
    /// Memory not mapped.
    NotMapped,
    /// Permission denied.
    PermissionDenied,
}

/// Error returned during register setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmSetRegistersError {
    VmcsGuard(&'static str),
    VmcsWrite(VmcsWriteError),
}

/// Error returned during register getting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmGetRegistersError {
    VmcsGuard(&'static str),
    VmcsRead(VmcsReadError),
}

/// Error from VM run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmRunError {
    /// Failed to prepare or restore the guest instruction counter.
    InstructionCounter(InstructionCounterError),
    /// VM entry failed (VMLAUNCH/VMRESUME error).
    VmEntry(VmEntryError),
    /// Exit handler encountered a fatal error.
    ExitHandler(ExitError),
    VmcsLoad(&'static str),
    VmcsClear(&'static str),
    WriteHostRsp(VmcsWriteError),
    ReadHostCr3,
    WriteHostCr3(VmcsWriteError),
    WriteHostFsBase(VmcsWriteError),
    WriteHostGsBase(VmcsWriteError),
    WriteHostTrBase(VmcsWriteError),
    WriteHostGdtrBase(VmcsWriteError),
    /// INVEPT of stale EPT TLB entries after cross-CPU migration failed.
    InveptFailed(InveptError),
}
