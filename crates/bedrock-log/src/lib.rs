// SPDX-License-Identifier: GPL-2.0

//! Conditional logging macros for bedrock.
//!
//! Kernel builds use the kernel's `pr_*!` macros; `cargo` builds compile to no-ops.
//!
//! # Usage
//!
//! ```ignore
//! use bedrock_log::{log_info, log_err, log_warn, log_debug};
//!
//! log_info!("Hello from bedrock!\n");
//! log_err!("An error occurred: {}\n", error_code);
//! log_warn!("Warning: value {} is deprecated\n", value);
//! log_debug!("Debug info: {:?}\n", data);
//! ```

#![no_std]

#[macro_use]
mod macros;
