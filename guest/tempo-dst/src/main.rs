// SPDX-License-Identifier: GPL-2.0

//! In-guest side of the Tempo deterministic simulation harness.
//!
//! ```text
//! tempo-dst start '<config json>'  # set up a branch; prints the assertion offset
//! tempo-dst nemesis    # crash/restart the node per /bedrock/in/config.json
//! tempo-dst redecide '<json>'  # replace the rest of a running branch's kill plan / load generations
//! tempo-dst oracle     # online oracles (log scan, liveness, durability)
//! tempo-dst finalize   # end-of-run oracles (graceful stop, re-execute)
//! tempo-dst head       # print the node's head block number
//! tempo-dst deploy <image> <address> [spec]  # deploy a load's contract
//! tempo-dst deploy-tip20 <image>      # create and mint the TIP-20 load's token
//! tempo-dst trie-spec <seed> <generation> <steps>  # print a generated trie load spec
//! tempo-dst tip20-spec <seed> <generation> <steps> # print a generated TIP-20 load spec
//! tempo-dst chain-spec <seed> <generation> <steps>  # print a generated chain load spec
//! tempo-dst chain-check [block]  # one-shot E9 at block (default: head)
//! tempo-dst chain-watch <secs>   # E9 against a live node, outside Bedrock
//! ```
//!
//! Assertions go to /bedrock/assertions.jsonl, control events to
//! /bedrock/events.jsonl. `TEMPO_DST_RPC` overrides the node's RPC URL.

mod cob;
mod cob_gen;
mod common;
mod finalize;
mod nemesis;
mod oracle;
mod reference;
mod start;
mod tip20;
mod trie_gen;
mod trie_ref;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("start") => {
            let config = std::env::args().nth(2).unwrap_or_default();
            if let Err(e) = start::run(&config) {
                eprintln!("start: {e}");
                std::process::exit(1);
            }
        }
        Some("nemesis") => nemesis::run(std::env::args().nth(2).as_deref() == Some("--resume")),
        Some("redecide") => {
            if let Err(e) = nemesis::redecide(&std::env::args().nth(2).unwrap_or_default()) {
                eprintln!("redecide: {e}");
                std::process::exit(1);
            }
        }
        Some("oracle") => oracle::run(),
        Some("finalize") => finalize::run(),
        Some("deploy") => {
            let arg = |i| std::env::args().nth(i).unwrap_or_default();
            let spec = std::env::args()
                .nth(4)
                .unwrap_or_else(|| start::TRIE_DEPLOY_SPEC.into());
            if let Err(e) = start::deploy(&arg(2), &arg(3), &spec) {
                eprintln!("deploy: {e}");
                std::process::exit(1);
            }
        }
        Some("deploy-tip20") => {
            if let Err(e) = start::deploy_tip20(&std::env::args().nth(2).unwrap_or_default()) {
                eprintln!("deploy-tip20: {e}");
                std::process::exit(1);
            }
        }
        Some("tip20-spec") => {
            let arg = |i| {
                std::env::args()
                    .nth(i)
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0)
            };
            print!("{}", tip20::spec(arg(2), arg(3), arg(4) as usize));
        }
        Some("trie-spec") => {
            let arg = |i| {
                std::env::args()
                    .nth(i)
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0)
            };
            let seed = arg(2);
            print!(
                "{}",
                trie_gen::spec(seed, arg(3), &trie_gen::slots(seed), arg(4) as usize)
            );
        }
        Some("chain-spec") => {
            let arg = |i| {
                std::env::args()
                    .nth(i)
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0)
            };
            print!("{}", cob_gen::spec(arg(2), arg(3), arg(4) as usize));
        }
        Some("chain-check") => {
            let mut chain = oracle::RpcChain;
            let block = match std::env::args().nth(2) {
                Some(b) => b.parse().ok(),
                None => common::head_number().ok(),
            };
            let Some(block) = block else {
                eprintln!("chain-check: no block");
                std::process::exit(1);
            };
            match cob::check_once(&mut chain, cob_gen::CONTRACT, block) {
                Ok((summary, verdicts)) => {
                    println!("{summary}");
                    for v in &verdicts {
                        println!("{v:?}");
                    }
                    if !verdicts.is_empty() {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("chain-check: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("chain-watch") => {
            let secs = std::env::args()
                .nth(2)
                .and_then(|a| a.parse().ok())
                .unwrap_or(60);
            cob::watch(&mut oracle::RpcChain, cob_gen::CONTRACT, secs);
        }
        Some("reference-head") => {
            match common::with_rpc_url(common::REFERENCE_RPC_URL, common::head_number) {
                Ok(n) => println!("{n}"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some("head") => match common::head_number() {
            Ok(n) => println!("{n}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage: tempo-dst <start|nemesis|redecide|oracle|finalize|head|reference-head|deploy|deploy-tip20|trie-spec|tip20-spec|chain-spec|chain-check|chain-watch>");
            std::process::exit(2);
        }
    }
}
