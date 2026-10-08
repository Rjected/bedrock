// SPDX-License-Identifier: GPL-2.0

//! In-guest side of the Tempo deterministic simulation harness.
//!
//! ```text
//! tempo-dst start '<config json>'  # set up a branch; prints the assertion offset
//! tempo-dst nemesis    # crash/restart the node per /bedrock/in/config.json
//! tempo-dst oracle     # online oracles (log scan, liveness, durability)
//! tempo-dst finalize   # end-of-run oracles (graceful stop, re-execute)
//! tempo-dst head       # print the node's head block number
//! ```
//!
//! Assertions go to /bedrock/assertions.jsonl, control events to
//! /bedrock/events.jsonl.

mod common;
mod finalize;
mod nemesis;
mod oracle;
mod start;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("start") => {
            let config = std::env::args().nth(2).unwrap_or_default();
            if let Err(e) = start::run(&config) {
                eprintln!("start: {e}");
                std::process::exit(1);
            }
        }
        Some("nemesis") => nemesis::run(),
        Some("oracle") => oracle::run(),
        Some("finalize") => finalize::run(),
        Some("head") => match common::head_number() {
            Ok(n) => println!("{n}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage: tempo-dst <start|nemesis|oracle|finalize|head>");
            std::process::exit(2);
        }
    }
}
