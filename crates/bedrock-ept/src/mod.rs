// SPDX-License-Identifier: GPL-2.0

//! Re-export for use as a submodule in kernel builds. `extern crate alloc`
//! must be declared at the kernel crate root (bedrock_main.rs).

#![allow(unreachable_pub, dead_code)]

mod compat;
mod entry;
mod table;
mod traits;

pub use entry::{EptMemoryType, EptPermissions, PageTableFormat};
pub use table::{EptPageTable, NptExecuteGuard, NptExecutionGuard, NptWriteGuard};
pub use traits::FrameAllocator;
