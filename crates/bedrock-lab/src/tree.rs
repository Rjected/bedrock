// SPDX-License-Identifier: GPL-2.0

//! Tree — a read-only view of the execution genealogy.

use std::sync::Arc;

use crate::branch::BranchId;
use crate::checkpoint::{Checkpoint, CheckpointId};
use crate::inner::{BranchMeta, LabInner};
use crate::time::VirtTime;

/// By-value snapshot of a live [`Branch`](crate::Branch)'s metadata.
#[derive(Debug, Clone)]
pub struct BranchView {
    pub id: BranchId,
    pub origin: CheckpointId,
    pub current_time: VirtTime,
}

/// A read-only snapshot of every live [`Checkpoint`] and
/// [`Branch`](crate::Branch); it doesn't extend their lifetimes.
pub struct Tree {
    pub(crate) checkpoints: Vec<Checkpoint>,
    pub(crate) branches: Vec<BranchView>,
}

impl Tree {
    pub(crate) fn from_lab(lab: &Arc<LabInner>) -> Self {
        let checkpoints = lab.graph.lock().unwrap().checkpoints();
        let mut branches: Vec<_> = lab
            .live_branches
            .lock()
            .unwrap()
            .values()
            .map(|m: &BranchMeta| BranchView {
                id: m.id,
                origin: m.origin,
                current_time: m.current_time,
            })
            .collect();
        branches.sort_by_key(|b| b.id);
        Self {
            checkpoints,
            branches,
        }
    }

    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    pub fn branches(&self) -> &[BranchView] {
        &self.branches
    }

    /// Render the tree as a Graphviz DOT graph.
    pub fn dot(&self) -> String {
        let mut s = String::new();
        s.push_str("digraph tree {\n");
        s.push_str("  rankdir=LR;\n");
        s.push_str("  node [shape=box];\n");
        for cp in &self.checkpoints {
            s.push_str(&format!(
                "  cp{} [label=\"cp{} @ {:.3}s\"];\n",
                cp.id().0,
                cp.id().0,
                cp.time().as_secs_f64()
            ));
        }
        for cp in &self.checkpoints {
            if let Some(parent) = cp.parent() {
                s.push_str(&format!("  cp{} -> cp{};\n", parent.id().0, cp.id().0));
            }
        }
        s.push_str("  node [shape=oval, style=dashed];\n");
        for b in &self.branches {
            s.push_str(&format!(
                "  br{} [label=\"br{} @ {:.3}s\"];\n",
                b.id.0,
                b.id.0,
                b.current_time.as_secs_f64()
            ));
            s.push_str(&format!("  cp{} -> br{};\n", b.origin.0, b.id.0));
        }
        s.push_str("}\n");
        s
    }
}
