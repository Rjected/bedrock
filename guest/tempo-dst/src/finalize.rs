// SPDX-License-Identifier: GPL-2.0

//! End-of-run oracles, invoked once by the host driver:
//!
//! - stops the nemesis, load, and online oracle so shutdown isn't judged as a
//!   fault;
//! - E4: stops the node gracefully and re-executes `[1, head]` from its datadir
//!   in a fresh container, which checks receipts, gas, and changesets against
//!   what the node persisted.
//! - E7: stops the reference node (if any) gracefully first.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bedrock_assertions::Condition;
use serde_json::json;

use crate::common::{self, Config, NODE_CONTAINER, REFERENCE_CONTAINER};

fn sh(cmd: &str) -> (bool, String) {
    match Command::new("sh").args(["-c", cmd]).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text)
        }
        Err(e) => (false, e.to_string()),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn run() {
    common::emit_event("driver", "finalize_start", None, json!({}));
    sh("pkill -f 'tempo-dst nemesis'; pkill -f 'tempo-dst oracle'");
    if sh("podman container exists txgen && test \"$(podman inspect txgen --format '{{.State.Running}}')\" = true").0 {
        // Logged as a nemesis kill so workload-monitor excuses the SIGKILL.
        common::emit_event("nemesis", "kill", Some("txgen"), json!({"reason": "finalize"}));
        sh("podman kill -s KILL txgen");
    }

    let head = common::head_number().ok();
    // Wait for the node to persist what it has before a graceful stop.
    std::thread::sleep(Duration::from_secs(5));
    let (_, image) = sh(&format!(
        "podman inspect {NODE_CONTAINER} --format '{{{{.ImageName}}}}'"
    ));
    let (_, volume) = sh(&format!(
        "podman inspect {NODE_CONTAINER} --format '{{{{range .Mounts}}}}{{{{if eq .Destination \"/data\"}}}}{{{{.Name}}}}{{{{end}}}}{{{{end}}}}'"
    ));
    let (image, volume) = (image.trim().to_string(), volume.trim().to_string());
    if Config::load().reference {
        let (stopped, out) = sh(&format!("podman stop -t 120 {REFERENCE_CONTAINER}"));
        common::assert_always(
            Condition::Bool(stopped),
            "finalize",
            "E7/reference-graceful-stop",
            &tail(&out, 5),
        );
    }
    let (stopped, stop_out) = sh(&format!("podman stop -t 120 {NODE_CONTAINER}"));
    common::assert_always(
        Condition::Bool(stopped),
        "finalize",
        "E4/graceful-stop",
        &tail(&stop_out, 5),
    );

    let chain = std::env::var("TEMPO_DST_CHAIN").unwrap_or_else(|_| "dev".into());
    let mut reexec = json!(null);
    if let Some(head) = head.filter(|h| *h > 1) {
        let cmd = format!(
            "podman run --rm --network none -v {volume}:/data --entrypoint /usr/local/bin/tempo {image} \
             re-execute --datadir /data --chain {chain} --from 1 --to {head} 2>&1"
        );
        let (ok, out) = sh(&cmd);
        let clean = ok && !out.contains("Invalid block") && !out.contains("mismatch");
        common::assert_always(
            Condition::Bool(clean),
            "finalize",
            "E4/re-execute",
            &format!("blocks 1..={head}\n{}", tail(&out, 40)),
        );
        common::assert_sometimes(Condition::Bool(ok), "finalize", "S/re-executed", "");
        reexec = json!({"ok": ok, "clean": clean, "output_tail": tail(&out, 200)});
    }

    let summary = json!({
        "head": head,
        "image": image,
        "volume": volume,
        "stopped": stopped,
        "re_execute": reexec,
    });
    let _ = std::fs::create_dir_all(common::OUT_DIR);
    let _ = std::fs::write(
        Path::new(common::OUT_DIR).join("finalize.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    );
    common::emit_event("oracle", "finalized", None, summary);
}
