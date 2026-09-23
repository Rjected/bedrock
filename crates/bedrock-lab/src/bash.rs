// SPDX-License-Identifier: GPL-2.0

//! Lab types over the I/O channel wire protocol ([`bedrock_vm::io_channel`]).

use bedrock_vm::io_channel;

/// Where a [`Branch::bash`](crate::Branch::bash) command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashTarget {
    /// Run on the guest host (outside any container).
    Host,
    Container(String),
}

impl BashTarget {
    pub fn host() -> Self {
        Self::Host
    }

    pub fn container(name: impl Into<String>) -> Self {
        Self::Container(name.into())
    }

    pub(crate) fn container_name(&self) -> Option<&str> {
        match self {
            BashTarget::Host => None,
            BashTarget::Container(name) => Some(name),
        }
    }
}

/// Result of a [`Branch::bash`](crate::Branch::bash) call.
#[derive(Debug, Clone)]
pub struct BashOutput {
    /// 0 = dispatched and run; negative = guest-module errno.
    pub status: i32,
    pub exit_code: i32,
    /// Combined stdout+stderr; empty unless recording was requested.
    pub output: Vec<u8>,
}

impl BashOutput {
    /// Dispatched and exited 0.
    pub fn success(&self) -> bool {
        self.status == 0 && self.exit_code == 0
    }

    pub fn output_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.output)
    }
}

pub(crate) fn encode_request(target: &BashTarget, cmd: &str, record_output: bool) -> Vec<u8> {
    io_channel::encode_request(target.container_name(), cmd, record_output)
}
