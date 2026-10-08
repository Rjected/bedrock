// SPDX-License-Identifier: GPL-2.0

//! Online oracles over the node's log stream, the DST event stream, and RPC:
//!
//! - E1 log scanner: panics, consensus mismatches, trie-update differences,
//!   engine/persistence failures, and ERROR-level lines.
//! - E2 liveness: the head advances within `liveness_secs` while the node is up.
//! - E3 durability: the block the node reported finalized before a kill keeps
//!   its hash after the restart, and the head returns to it. Unfinalized blocks
//!   may be rebuilt: reth's crash recovery unwinds to its persisted state-trie
//!   frontier, which can trail the "Saved range of blocks" frontier.
//!
//! [`Oracle`] is a pure state machine; [`run`] feeds it from the guest.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use bedrock_assertions::Condition;

use crate::common::{self, Config, DstEvent, NODE_CONTAINER};

/// Signatures for log lines that indicate a bug on their own. Message strings
/// are taken from reth at Tempo's pinned rev (42fa3c5); see the README table.
const LOG_PATTERNS: &[(&str, &str)] = &[
    ("E1/panic", "panicked at"),
    ("E1/state-root-mismatch", "mismatched block state root"),
    ("E1/receipt-root-mismatch", "receipt root mismatch"),
    ("E1/gas-used-mismatch", "block gas used mismatch"),
    ("E1/tx-root-mismatch", "mismatched block transaction root"),
    ("E1/bloom-mismatch", "header bloom filter mismatch"),
    ("E1/trie-diff-account", "Difference in account trie updates"),
    ("E1/trie-diff-storage", "Difference in storage trie updates"),
    (
        "E1/trie-diff-removed-account",
        "Difference in removed account trie nodes",
    ),
    (
        "E1/trie-diff-removed-storage",
        "Difference in removed storage trie nodes",
    ),
    ("E1/engine-fatal", "Fatal error in consensus engine"),
    ("E1/insert-fatal", "insert block fatal error"),
    ("E1/connect-fatal", "fatal error occurred while"),
    ("E1/persistence-failed", "Persistence service failed"),
    (
        "E1/persistence-advance-failed",
        "Advancing persistence failed",
    ),
    ("E1/persistence-poll-failed", "Polling persistence failed"),
    (
        "E1/persistence-complete-failed",
        "Persistence complete handling failed",
    ),
    (
        "E1/insert-executed-failed",
        "Failed to insert already executed block",
    ),
    ("E1/tree-state-missing", "block not found in TreeState"),
    ("E1/bad-block", "Bad block"),
    ("E1/invalid-payload", "Invalid payload"),
    ("E1/unwind-failed", "failed to run unwind"),
];

const SAVED_BLOCKS: &str = "Saved range of blocks";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub signature: String,
    pub detail: String,
}

/// Strips ANSI escape sequences reth may emit.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// E1: the failing signature for one log line, if any.
pub fn scan_line(line: &str) -> Option<Finding> {
    let line = strip_ansi(line);
    let detail: String = line.chars().take(400).collect();
    if let Some((sig, _)) = LOG_PATTERNS.iter().find(|(_, p)| line.contains(p)) {
        return Some(Finding {
            signature: sig.to_string(),
            detail,
        });
    }
    // reth's default format: "<ts>  ERROR <target>: <message> <fields>".
    let rest = line.split_once(" ERROR ")?.1;
    let target = rest.split(": ").next().unwrap_or("").trim();
    Some(Finding {
        signature: format!("E1/error-log/{target}"),
        detail,
    })
}

/// Parses `last=NumHash { number: N, hash: 0x.. }` from a "Saved range of
/// blocks" line. Block data up to `last` is committed, but the state trie may
/// lag it, so a crash can still unwind these blocks (see E3).
pub fn parse_saved(line: &str) -> Option<(u64, String)> {
    let line = strip_ansi(line);
    if !line.contains(SAVED_BLOCKS) {
        return None;
    }
    let last = line.split_once("last=")?.1;
    let number = last.split_once("number: ")?.1;
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    let hash = last.split_once("hash: ")?.1;
    let hash: String = hash
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    Some((digits.parse().ok()?, hash.to_lowercase()))
}

/// Outputs of one oracle step, written as assertions by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// An Always assertion with the given outcome.
    Always(bool, Finding),
    /// A Sometimes assertion (coverage signal) with the given outcome.
    Sometimes(bool, Finding),
}

fn finding(signature: &str, detail: String) -> Finding {
    Finding {
        signature: signature.into(),
        detail,
    }
}

/// Node queries; `Err` while the node is unreachable.
pub trait Chain {
    fn head(&mut self) -> Result<u64, String>;
    fn hash(&mut self, number: u64) -> Result<Option<String>, String>;
    fn finalized(&mut self) -> Result<Option<(u64, String)>, String>;
    fn tx_count(&mut self, block: u64) -> Result<u64, String>;
}

#[derive(Debug)]
pub struct Oracle {
    liveness_ns: u64,
    /// Latest "Saved range of blocks" frontier.
    saved: Option<(u64, String)>,
    /// Latest finalized block seen while the node was up.
    finalized: Option<(u64, String)>,
    /// Head and when it last advanced.
    head: Option<u64>,
    progress_ns: u64,
    stall_reported: bool,
    /// Node killed and not yet restarted.
    down: bool,
    /// Finalized block captured at the last kill, awaiting verification.
    pending_finalized: Option<(u64, String)>,
    /// Saved frontier captured at the last kill, for the rewind coverage signal.
    pending_saved: Option<(u64, String)>,
    /// Head when the last kill happened, for the recovery coverage signal.
    head_at_kill: Option<u64>,
    restart_ns: u64,
    /// A block produced during the run included a transaction.
    load_seen: bool,
}

impl Oracle {
    pub fn new(liveness_secs: u64, now_ns: u64) -> Oracle {
        Oracle {
            liveness_ns: liveness_secs * 1_000_000_000,
            saved: None,
            finalized: None,
            head: None,
            progress_ns: now_ns,
            stall_reported: false,
            down: false,
            pending_finalized: None,
            pending_saved: None,
            head_at_kill: None,
            restart_ns: now_ns,
            load_seen: false,
        }
    }

    pub fn on_log(&mut self, line: &str) -> Vec<Verdict> {
        if let Some(saved) = parse_saved(line) {
            self.saved = Some(saved);
            return Vec::new();
        }
        scan_line(line)
            .map(|f| vec![Verdict::Always(false, f)])
            .unwrap_or_default()
    }

    pub fn on_event(&mut self, ev: &DstEvent, now_ns: u64) -> Vec<Verdict> {
        if ev.source != "nemesis" || ev.container.as_deref() != Some(NODE_CONTAINER) {
            return Vec::new();
        }
        match ev.kind.as_str() {
            "kill" => {
                self.down = true;
                self.pending_finalized = self.finalized.clone();
                self.pending_saved = self.saved.clone();
                self.head_at_kill = self.head;
                vec![Verdict::Sometimes(
                    true,
                    finding(
                        "S/kill",
                        format!("finalized={:?} saved={:?}", self.finalized, self.saved),
                    ),
                )]
            }
            "restart" => {
                self.down = false;
                self.restart_ns = now_ns;
                self.progress_ns = now_ns;
                self.stall_reported = false;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub fn tick(&mut self, chain: &mut impl Chain, now_ns: u64) -> Vec<Verdict> {
        let mut out = Vec::new();
        if self.down {
            return out;
        }
        if let Ok(head) = chain.head() {
            if self.head.is_some_and(|h| head > h)
                && !self.load_seen
                && chain.tx_count(head).is_ok_and(|n| n > 0)
            {
                self.load_seen = true;
                out.push(Verdict::Sometimes(
                    true,
                    finding("S/load-included", format!("block {head}")),
                ));
            }
            if self.head.is_none_or(|h| head > h) {
                self.progress_ns = now_ns;
                self.stall_reported = false;
                if let Some(k) = self.head_at_kill {
                    if head > k {
                        self.head_at_kill = None;
                        out.push(Verdict::Sometimes(
                            true,
                            finding(
                                "S/recovered",
                                format!("head {head} passed pre-kill head {k}"),
                            ),
                        ));
                    }
                }
            }
            self.head = Some(head);
            if let Ok(Some(f)) = chain.finalized() {
                self.finalized = Some(f);
            }
            if let Some((n, want)) = self.pending_finalized.clone() {
                if head >= n {
                    if let Ok(got) = chain.hash(n) {
                        let ok = got.as_deref().map(str::to_lowercase).as_deref() == Some(&want);
                        out.push(Verdict::Always(
                            ok,
                            finding(
                                "E3/finalized-block-changed",
                                format!("block {n}: finalized {want}, after restart {got:?}"),
                            ),
                        ));
                        self.pending_finalized = None;
                    }
                }
            }
            if let Some((n, want)) = self.pending_saved.clone() {
                if head >= n {
                    if let Ok(got) = chain.hash(n) {
                        let rewound =
                            got.as_deref().map(str::to_lowercase).as_deref() != Some(&want);
                        out.push(Verdict::Sometimes(
                            rewound,
                            finding(
                                "S/rewound-unfinalized",
                                format!("saved block {n}: {want} -> {got:?}"),
                            ),
                        ));
                        self.pending_saved = None;
                    }
                }
            }
        }
        if let Some((n, _)) = &self.pending_finalized {
            if now_ns.saturating_sub(self.restart_ns) > self.liveness_ns {
                out.push(Verdict::Always(
                    false,
                    finding(
                        "E3/head-below-finalized",
                        format!(
                            "head {:?} never reached finalized block {n} after restart",
                            self.head
                        ),
                    ),
                ));
                self.pending_finalized = None;
            }
        }
        if !self.stall_reported && now_ns.saturating_sub(self.progress_ns) > self.liveness_ns {
            self.stall_reported = true;
            out.push(Verdict::Always(
                false,
                finding(
                    "E2/head-stalled",
                    format!(
                        "head {:?} did not advance for {}s",
                        self.head,
                        self.liveness_ns / 1_000_000_000
                    ),
                ),
            ));
        }
        out
    }
}

struct RpcChain;

impl Chain for RpcChain {
    fn head(&mut self) -> Result<u64, String> {
        common::head_number()
    }
    fn hash(&mut self, number: u64) -> Result<Option<String>, String> {
        common::block_hash(number)
    }
    fn finalized(&mut self) -> Result<Option<(u64, String)>, String> {
        common::finalized_block()
    }
    fn tx_count(&mut self, block: u64) -> Result<u64, String> {
        common::tx_count(block)
    }
}

fn record(verdicts: Vec<Verdict>) {
    for v in verdicts {
        match v {
            Verdict::Always(ok, f) => {
                common::assert_always(Condition::Bool(ok), "oracle", &f.signature, &f.detail)
            }
            Verdict::Sometimes(ok, f) => {
                common::assert_sometimes(Condition::Bool(ok), "oracle", &f.signature, &f.detail)
            }
        }
    }
}

/// Follows the node's container output across restarts; journald keeps one
/// stream per container name.
fn follow_logs(tx: mpsc::Sender<String>) {
    let child = Command::new("journalctl")
        .args(["-f", "-o", "cat", "--no-tail"])
        .arg(format!("CONTAINER_NAME={NODE_CONTAINER}"))
        .stdout(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("cannot spawn journalctl");
        return;
    };
    let stdout = child.stdout.take().expect("piped stdout");
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if tx.send(line).is_err() {
            break;
        }
    }
}

pub fn run() {
    let cfg = Config::load();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || follow_logs(tx));
    let mut oracle = Oracle::new(cfg.liveness_secs, common::guest_time_ns());
    let mut chain = RpcChain;
    let mut seen_events = 0;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = common::guest_time_ns();
        let mut out = Vec::new();
        for line in rx.try_iter() {
            out.extend(oracle.on_log(&line));
        }
        let events = common::read_events();
        for ev in events.iter().skip(seen_events) {
            out.extend(oracle.on_event(ev, now));
        }
        seen_events = events.len();
        out.extend(oracle.tick(&mut chain, now));
        record(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const S: u64 = 1_000_000_000;

    #[derive(Default)]
    struct FakeChain {
        head: Option<u64>,
        finalized: Option<u64>,
        hashes: std::collections::HashMap<u64, String>,
        tx_counts: std::collections::HashMap<u64, u64>,
    }

    impl Chain for FakeChain {
        fn head(&mut self) -> Result<u64, String> {
            self.head.ok_or_else(|| "down".into())
        }
        fn hash(&mut self, n: u64) -> Result<Option<String>, String> {
            Ok(self.hashes.get(&n).cloned())
        }
        fn finalized(&mut self) -> Result<Option<(u64, String)>, String> {
            Ok(self
                .finalized
                .map(|n| (n, self.hashes.get(&n).cloned().unwrap_or_default())))
        }
        fn tx_count(&mut self, block: u64) -> Result<u64, String> {
            Ok(self.tx_counts.get(&block).copied().unwrap_or(0))
        }
    }

    #[test]
    fn included_load_is_covered_once() {
        let mut o = Oracle::new(60, 0);
        let mut c = FakeChain {
            head: Some(10),
            ..Default::default()
        };
        // The head at the first tick predates the run.
        c.tx_counts.insert(10, 1);
        assert!(sigs(&o.tick(&mut c, S)).is_empty());
        c.head = Some(11);
        assert!(sigs(&o.tick(&mut c, 2 * S)).is_empty());
        c.head = Some(12);
        c.tx_counts.insert(12, 3);
        assert_eq!(sigs(&o.tick(&mut c, 3 * S)), [(true, "S/load-included")]);
        c.head = Some(13);
        c.tx_counts.insert(13, 3);
        assert!(sigs(&o.tick(&mut c, 4 * S)).is_empty());
    }

    /// The shape of the native run's second crash: block data saved through
    /// 318, finalized at 269, head 330 at the kill.
    fn chain_before_kill() -> FakeChain {
        let mut c = FakeChain {
            head: Some(330),
            finalized: Some(269),
            ..Default::default()
        };
        c.hashes.insert(269, "0xf1".into());
        c.hashes.insert(318, "0x318a".into());
        c
    }

    const SAVED_318: &str = "DEBUG engine::persistence: Saved range of blocks first=Some(NumHash { number: 308, hash: 0x1 }) last=NumHash { number: 318, hash: 0x318a }";

    fn ev(kind: &str) -> DstEvent {
        DstEvent {
            source: "nemesis".into(),
            kind: kind.into(),
            container: Some(NODE_CONTAINER.into()),
            guest_time_ns: 0,
            detail: Value::Null,
        }
    }

    fn sigs(v: &[Verdict]) -> Vec<(bool, &str)> {
        v.iter()
            .map(|v| match v {
                Verdict::Always(ok, f) | Verdict::Sometimes(ok, f) => (*ok, f.signature.as_str()),
            })
            .collect()
    }

    const SAVED: &str = "2026-10-07T00:00:00.000000Z DEBUG engine::persistence: Saved range of blocks first=Some(NumHash { number: 5, hash: 0xaa }) last=NumHash { number: 7, hash: 0xBEEF01 }";

    #[test]
    fn parses_saved_range() {
        assert_eq!(parse_saved(SAVED), Some((7, "0xbeef01".into())));
        assert_eq!(
            parse_saved("Saving range of blocks last=NumHash { number: 9, hash: 0x1 }"),
            None
        );
    }

    #[test]
    fn parses_saved_range_with_ansi() {
        let colored = SAVED.replace("DEBUG", "\x1b[34mDEBUG\x1b[0m");
        assert_eq!(parse_saved(&colored), Some((7, "0xbeef01".into())));
    }

    #[test]
    fn scans_known_patterns_and_errors() {
        let panic = "thread 'tokio-runtime-worker' panicked at crates/trie/sparse/src/lib.rs:10:5:";
        assert_eq!(scan_line(panic).unwrap().signature, "E1/panic");
        let diff = "WARN engine::tree: Difference in account trie updates path=0x1";
        assert_eq!(scan_line(diff).unwrap().signature, "E1/trie-diff-account");
        let err = "2026-10-07T00:00:00Z ERROR engine::tree: something odd";
        assert_eq!(
            scan_line(err).unwrap().signature,
            "E1/error-log/engine::tree"
        );
        assert_eq!(scan_line("INFO reth::cli: Status connected_peers=0"), None);
    }

    #[test]
    fn saved_line_is_not_a_finding() {
        let mut o = Oracle::new(60, 0);
        assert!(o.on_log(SAVED).is_empty());
    }

    #[test]
    fn rewinding_unfinalized_blocks_is_coverage_not_failure() {
        // reth unwinds to its state-trie frontier (308) and rebuilds 309..318.
        let mut o = Oracle::new(60, 0);
        let mut c = chain_before_kill();
        o.on_log(SAVED_318);
        o.tick(&mut c, S);
        assert_eq!(sigs(&o.on_event(&ev("kill"), 2 * S)), [(true, "S/kill")]);
        c.head = None;
        assert!(o.tick(&mut c, 3 * S).is_empty());
        o.on_event(&ev("restart"), 4 * S);
        c.head = Some(308);
        assert_eq!(
            sigs(&o.tick(&mut c, 5 * S)),
            [(true, "E3/finalized-block-changed")]
        );
        c.hashes.insert(318, "0x318b".into());
        c.head = Some(320);
        assert_eq!(
            sigs(&o.tick(&mut c, 6 * S)),
            [(true, "S/rewound-unfinalized")]
        );
        c.head = Some(331);
        assert_eq!(sigs(&o.tick(&mut c, 7 * S)), [(true, "S/recovered")]);
    }

    #[test]
    fn durability_fails_when_finalized_block_changes() {
        let mut o = Oracle::new(60, 0);
        let mut c = chain_before_kill();
        o.tick(&mut c, S);
        o.on_event(&ev("kill"), 2 * S);
        o.on_event(&ev("restart"), 3 * S);
        c.hashes.insert(269, "0xother".into());
        assert!(sigs(&o.tick(&mut c, 4 * S)).contains(&(false, "E3/finalized-block-changed")));
    }

    #[test]
    fn durability_fails_when_head_never_reaches_finalized() {
        let mut o = Oracle::new(10, 0);
        let mut c = chain_before_kill();
        o.tick(&mut c, S);
        o.on_event(&ev("kill"), 2 * S);
        o.on_event(&ev("restart"), 3 * S);
        c.head = Some(200);
        assert!(!sigs(&o.tick(&mut c, 4 * S))
            .iter()
            .any(|(_, s)| s.starts_with("E3/")));
        let v = o.tick(&mut c, 14 * S);
        assert!(sigs(&v).contains(&(false, "E3/head-below-finalized")));
    }

    #[test]
    fn no_finalized_block_means_nothing_to_check() {
        let mut o = Oracle::new(10, 0);
        let mut c = FakeChain {
            head: Some(5),
            ..Default::default()
        };
        o.tick(&mut c, S);
        o.on_event(&ev("kill"), 2 * S);
        o.on_event(&ev("restart"), 3 * S);
        assert!(!sigs(&o.tick(&mut c, 20 * S))
            .iter()
            .any(|(_, s)| s.starts_with("E3/")));
    }

    #[test]
    fn liveness_fails_once_per_stall_and_ignores_downtime() {
        let mut o = Oracle::new(10, 0);
        let mut c = FakeChain {
            head: Some(1),
            ..Default::default()
        };
        o.tick(&mut c, S);
        assert!(o.tick(&mut c, 5 * S).is_empty());
        assert_eq!(sigs(&o.tick(&mut c, 12 * S)), [(false, "E2/head-stalled")]);
        assert!(o.tick(&mut c, 30 * S).is_empty());
        c.head = Some(2);
        assert!(o.tick(&mut c, 31 * S).is_empty());
        o.on_event(&ev("kill"), 32 * S);
        assert!(o.tick(&mut c, 100 * S).is_empty());
        o.on_event(&ev("restart"), 100 * S);
        assert!(o.tick(&mut c, 105 * S).is_empty());
        assert_eq!(sigs(&o.tick(&mut c, 111 * S)), [(false, "E2/head-stalled")]);
    }
}
