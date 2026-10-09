// SPDX-License-Identifier: GPL-2.0

//! `tempo-dst warmup '<json>'`: the workload contract's warmup hook, run in
//! the guest by `bedrock-dst` after boot, every 5 virtual seconds, until it
//! prints `{"ready": true, ...}`; the driver then takes the warm checkpoint.
//!
//! Each call does what it can and returns: wait for the node to reach
//! `warm_blocks`, then (once) deploy the load's contracts or mint its TIP-20
//! token, so branches start loading at once, then, with `reference`, wait
//! for the reference node to backfill the primary's chain (seen: a reference
//! that never backfilled stalled at head 0 all run). A failed deploy is an
//! error (exit 1); a node that is not up yet is just not ready.

use std::fs;
use std::io;
use std::path::Path;

use bedrock_dst_contract::{WarmupRequest, WarmupStatus};

use crate::common;
use crate::planner::{
    Load, TempoArgs, CHAIN_CONTRACT, CHAIN_IMAGE, TIP20_IMAGE, TRIE_CONTRACT, TRIE_IMAGE,
};
use crate::start;

/// Blocks the reference may trail the primary at the warm checkpoint.
const REFERENCE_SYNC_SLACK: u64 = 2;
/// Marks the load's deploy done, across calls (and in every branch).
const DEPLOYED_PATH: &str = "/bedrock/warmup-deployed";

fn head(rpc: Option<&'static str>) -> Option<u64> {
    match rpc {
        Some(url) => common::with_rpc_url(url, common::head_number).ok(),
        None => common::head_number().ok(),
    }
}

fn status(ready: bool, detail: String) -> WarmupStatus {
    WarmupStatus { ready, detail }
}

/// One warmup step.
pub fn run(req: &WarmupRequest<TempoArgs>) -> io::Result<WarmupStatus> {
    let wa = &req.args;
    let primary = head(None);
    if !primary.is_some_and(|h| h >= req.warmup.warm_blocks) {
        return Ok(status(
            false,
            format!("head {primary:?} < {}", req.warmup.warm_blocks),
        ));
    }
    if !Path::new(DEPLOYED_PATH).exists() {
        let what = match wa.load {
            Load::Transfers => None,
            Load::Trie => {
                start::deploy(TRIE_IMAGE, TRIE_CONTRACT, "/workload/trie/deploy.yaml")
                    .map_err(|e| io::Error::other(format!("{TRIE_CONTRACT} deploy failed: {e}")))?;
                Some(format!("{TRIE_CONTRACT} deployed"))
            }
            Load::Chain => {
                start::deploy(CHAIN_IMAGE, CHAIN_CONTRACT, "/workload/chain/deploy.yaml").map_err(
                    |e| io::Error::other(format!("{CHAIN_CONTRACT} deploy failed: {e}")),
                )?;
                Some(format!("{CHAIN_CONTRACT} deployed"))
            }
            Load::Tip20 => {
                start::deploy_tip20(TIP20_IMAGE)
                    .map_err(|e| io::Error::other(format!("TIP-20 token deploy failed: {e}")))?;
                Some("TIP-20 token minted".to_string())
            }
        };
        fs::create_dir_all(Path::new(DEPLOYED_PATH).parent().unwrap())?;
        fs::write(
            DEPLOYED_PATH,
            what.as_deref().unwrap_or("nothing to deploy"),
        )?;
        if let Some(what) = what {
            // Report the deploy on its own line; the next call goes on.
            return Ok(status(false, what));
        }
    }
    if wa.reference {
        let primary = head(None);
        let reference = head(Some(common::REFERENCE_RPC_URL));
        let synced =
            matches!((primary, reference), (Some(p), Some(r)) if r + REFERENCE_SYNC_SLACK >= p);
        return Ok(status(
            synced,
            format!("head {primary:?}, reference head {reference:?}"),
        ));
    }
    Ok(status(true, format!("head {primary:?}")))
}
