// SPDX-License-Identifier: GPL-2.0

//! Per-VM anonymous-inode file descriptors (created by CREATE_ROOT_VM and
//! CREATE_FORKED_VM); the VM is released when its FD is closed.
//!
//! # Module Structure
//!
//! - [`structs`] - User ABI structures and ioctl definitions
//! - [`core`] - BedrockVmFile and BedrockForkedVmFile structs
//! - [`handlers`] - Shared trait-based ioctl handlers
//! - [`root`] - Root VM file operations
//! - [`forked`] - Forked VM file operations
//! - [`fd`] - FD creation functions

pub(crate) mod core;
pub(crate) mod fd;
pub(crate) mod forked;
pub(crate) mod handlers;
pub(crate) mod root;
pub(crate) mod structs;

pub(crate) use core::ParentVmArc;
pub(crate) use fd::{create_forked_vm_fd, create_vm_fd};
