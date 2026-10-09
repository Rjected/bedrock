// SPDX-License-Identifier: GPL-2.0

//! `seed-N/manifest.json`: what an input tape is tied to.
//!
//! A tape replays only against the binaries it was recorded with: the guest
//! kernel, initrd, image archive and compose file (sha256 each), the loaded
//! `bedrock.ko`, the TSC frequency and the boot seed, and it must start at
//! the same warm-checkpoint virtual time. `replay --tape` re-checks all of
//! these and refuses on a mismatch unless `--force`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use bedrock_lab::InputRecording;
use bedrock_vm::events::RandomSource;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const MANIFEST_VERSION: u32 = 1;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// One input file the tape depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl InputFile {
    pub fn of(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            sha256: sha256_file(path)?,
            bytes: std::fs::metadata(path)?.len(),
        })
    }
}

/// The loaded `bedrock.ko`, as far as it can be identified without root:
/// its `srcversion` (when the module exports one) and the sha256 of its file
/// (`$BEDROCK_KO`, else `modinfo -F filename bedrock`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleIdentity {
    pub srcversion: Option<String>,
    pub path: Option<String>,
    pub sha256: Option<String>,
}

impl ModuleIdentity {
    pub fn probe() -> Self {
        let srcversion = std::fs::read_to_string("/sys/module/bedrock/srcversion")
            .ok()
            .map(|s| s.trim().to_string());
        let path = std::env::var("BEDROCK_KO").ok().or_else(|| {
            let out = Command::new("modinfo")
                .args(["-F", "filename", "bedrock"])
                .output()
                .ok()?;
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !p.is_empty()).then_some(p)
        });
        let sha256 = path.as_deref().and_then(|p| sha256_file(Path::new(p)).ok());
        Self {
            srcversion,
            path,
            sha256,
        }
    }

    /// Mismatches on the fields both sides know.
    fn diff(&self, now: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (what, a, b) in [
            ("bedrock.ko srcversion", &self.srcversion, &now.srcversion),
            ("bedrock.ko sha256", &self.sha256, &now.sha256),
        ] {
            if let (Some(a), Some(b)) = (a, b) {
                if a != b {
                    out.push(format!("{what}: recorded {a}, loaded {b}"));
                }
            }
        }
        out
    }
}

/// Per-consumer randomness totals, keyed by `rdrand`/`rdseed` or
/// `getrandom:<pid>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerStats {
    pub draws: u64,
    pub bytes: u64,
}

pub fn consumers(rec: &InputRecording) -> BTreeMap<String, ConsumerStats> {
    let mut out: BTreeMap<String, ConsumerStats> = BTreeMap::new();
    for r in rec.random_inputs() {
        let key = match r.source {
            RandomSource::Rdrand => "rdrand".to_string(),
            RandomSource::Rdseed => "rdseed".to_string(),
            RandomSource::GetRandom => format!("getrandom:{}", r.pid),
        };
        let e = out.entry(key).or_default();
        e.draws += 1;
        e.bytes += r.bytes.len() as u64;
    }
    out
}

/// The tape file and what is on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapeSummary {
    pub file: String,
    pub sha256: String,
    pub random_inputs: u64,
    pub random_bytes: u64,
    pub io_inputs: u64,
    pub consumers: BTreeMap<String, ConsumerStats>,
}

/// Campaign-wide part of a manifest, computed once per campaign.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub vmlinux: InputFile,
    pub initrd: InputFile,
    pub images: InputFile,
    pub compose: InputFile,
    /// Images in the archive: tags, config digest and labels (e.g.
    /// `org.opencontainers.image.revision` when the build sets it).
    pub image_metadata: Vec<Value>,
    pub bedrock_ko: ModuleIdentity,
    /// `--build-info KEY=VALUE` pairs (e.g. Tempo/reth revisions).
    pub build_info: BTreeMap<String, String>,
    pub tsc_frequency: u64,
    pub boot_seed: u64,
}

impl Environment {
    pub fn probe(
        vmlinux: &Path,
        initrd: &Path,
        images: &Path,
        compose: &Path,
        build_info: &[String],
        tsc_frequency: u64,
        boot_seed: u64,
    ) -> io::Result<Self> {
        Ok(Self {
            vmlinux: InputFile::of(vmlinux)?,
            initrd: InputFile::of(initrd)?,
            images: InputFile::of(images)?,
            compose: InputFile::of(compose)?,
            image_metadata: image_metadata(images).unwrap_or_else(|e| {
                eprintln!("warning: cannot read image metadata from {images:?}: {e}");
                Vec::new()
            }),
            bedrock_ko: ModuleIdentity::probe(),
            build_info: build_info
                .iter()
                .map(|kv| match kv.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => (kv.clone(), String::new()),
                })
                .collect(),
            tsc_frequency,
            boot_seed,
        })
    }

    /// Everything about `now` that would make a recorded tape replay
    /// against different binaries.
    pub fn diff(&self, now: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (what, a, b) in [
            ("vmlinux", &self.vmlinux, &now.vmlinux),
            ("initrd", &self.initrd, &now.initrd),
            ("images", &self.images, &now.images),
            ("compose", &self.compose, &now.compose),
        ] {
            if a.sha256 != b.sha256 {
                out.push(format!(
                    "{what}: recorded {} ({}), now {} ({})",
                    a.sha256, a.path, b.sha256, b.path
                ));
            }
        }
        out.extend(self.bedrock_ko.diff(&now.bedrock_ko));
        if self.tsc_frequency != now.tsc_frequency {
            out.push(format!(
                "TSC frequency: recorded {}, now {}",
                self.tsc_frequency, now.tsc_frequency
            ));
        }
        if self.boot_seed != now.boot_seed {
            out.push(format!(
                "boot seed: recorded {}, now {}",
                self.boot_seed, now.boot_seed
            ));
        }
        out
    }
}

/// `seed-N/manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub seed: u64,
    pub bedrock_dst: Value,
    pub environment: Environment,
    /// `campaign.json` as the campaign ran.
    pub campaign: Value,
    /// Warm checkpoint (= branch start) and branch end, in retired guest
    /// instructions.
    pub warm_checkpoint_instructions: u64,
    pub branch_end_instructions: u64,
    /// `Branch::reseed_rng` seed of the original run (unused by a tape
    /// replay, which serves the tape instead).
    pub rng_seed: u64,
    /// `Branch::set_preempt` (period, seed), applied by every replay.
    pub preempt: Value,
    pub tape: Option<TapeSummary>,
    /// Set for a branch from a moment of another recorded run (`bedrock-dst
    /// branch`): its parent, moment and what it varied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<crate::branching::BranchInfo>,
}

pub fn bedrock_dst_identity() -> Value {
    let exe = std::env::current_exe().ok();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "exe": exe.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "exe_sha256": exe.as_deref().and_then(|p| sha256_file(p).ok()),
    })
}

/// Entries of a tar archive: `(name, data offset, size)`. Seeks over data,
/// so a multi-GB image archive costs one read per header.
fn tar_entries(f: &mut File) -> io::Result<Vec<(String, u64, u64)>> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut long_name: Option<String> = None;
    let mut header = [0u8; 512];
    loop {
        f.seek(SeekFrom::Start(pos))?;
        if f.read_exact(&mut header).is_err() || header.iter().all(|b| *b == 0) {
            return Ok(out);
        }
        let field = |r: std::ops::Range<usize>| {
            let s = &header[r];
            let end = s.iter().position(|b| *b == 0).unwrap_or(s.len());
            String::from_utf8_lossy(&s[..end]).into_owned()
        };
        let size = if header[124] & 0x80 != 0 {
            header[125..136]
                .iter()
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
        } else {
            u64::from_str_radix(field(124..136).trim(), 8).unwrap_or(0)
        };
        let mut name = field(0..100);
        if &header[257..262] == b"ustar" {
            let prefix = field(345..500);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let data = pos + 512;
        match header[156] {
            // GNU long name / pax header: applies to the next entry.
            b'L' | b'x' => {
                let mut buf = vec![0u8; size.min(1 << 16) as usize];
                f.seek(SeekFrom::Start(data))?;
                f.read_exact(&mut buf)?;
                let text = String::from_utf8_lossy(&buf).into_owned();
                long_name = if header[156] == b'L' {
                    Some(text.trim_end_matches('\0').to_string())
                } else {
                    text.lines()
                        .find_map(|l| l.split_once(" path=").map(|(_, p)| p.to_string()))
                };
            }
            _ => out.push((long_name.take().unwrap_or(name), data, size)),
        }
        pos = data + size.div_ceil(512) * 512;
    }
}

/// Tags, config digest and labels of each image in a `docker save` archive.
pub fn image_metadata(path: &Path) -> io::Result<Vec<Value>> {
    let mut f = File::open(path)?;
    let entries = tar_entries(&mut f)?;
    let mut read = |name: &str| -> io::Result<Option<Vec<u8>>> {
        let name = name.trim_start_matches("./");
        let Some((_, off, size)) = entries
            .iter()
            .find(|(n, _, _)| n.trim_start_matches("./") == name)
        else {
            return Ok(None);
        };
        if *size > 16 << 20 {
            return Ok(None);
        }
        let mut buf = vec![0u8; *size as usize];
        f.seek(SeekFrom::Start(*off))?;
        f.read_exact(&mut buf)?;
        Ok(Some(buf))
    };
    let Some(manifest) = read("manifest.json")? else {
        return Ok(Vec::new());
    };
    let manifest: Vec<Value> = serde_json::from_slice(&manifest).map_err(io::Error::other)?;
    let mut out = Vec::new();
    for image in manifest {
        let config = image["Config"].as_str().unwrap_or_default().to_string();
        let blob = read(&config)?;
        let labels = blob
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .map(|c| c["config"]["Labels"].clone())
            .unwrap_or(Value::Null);
        out.push(json!({
            "tags": image["RepoTags"],
            "config": config,
            "config_sha256": blob.as_deref().map(sha256_bytes),
            "labels": labels,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_header(name: &str, size: usize, kind: u8) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let sz = format!("{size:011o}");
        h[124..135].copy_from_slice(sz.as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h
    }

    fn tar_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        out.extend(tar_header(name, data.len(), b'0'));
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }

    #[test]
    fn reads_image_tags_and_labels_from_a_docker_archive() {
        let dir = std::env::temp_dir().join(format!("bedrock-dst-tar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("images.tar");
        let config = br#"{"config": {"Labels": {"org.opencontainers.image.revision": "abc123"}}}"#;
        let mut tar = Vec::new();
        tar_entry(&mut tar, "blobs/sha256/layer", &[7u8; 1000]);
        tar_entry(&mut tar, "blobs/sha256/cfg", config);
        tar_entry(
            &mut tar,
            "manifest.json",
            br#"[{"Config": "blobs/sha256/cfg", "RepoTags": ["bedrock/tempo-localnet:pinned"], "Layers": []}]"#,
        );
        tar.extend([0u8; 1024]);
        std::fs::write(&path, &tar).unwrap();
        let meta = image_metadata(&path).unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0]["tags"][0], "bedrock/tempo-localnet:pinned");
        assert_eq!(
            meta[0]["labels"]["org.opencontainers.image.revision"],
            "abc123"
        );
        assert_eq!(meta[0]["config_sha256"], sha256_bytes(config));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn environment_diff_names_every_mismatch() {
        let file = |sha: &str| InputFile {
            path: "p".into(),
            sha256: sha.into(),
            bytes: 1,
        };
        let env = Environment {
            vmlinux: file("a"),
            initrd: file("b"),
            images: file("c"),
            compose: file("d"),
            image_metadata: vec![],
            bedrock_ko: ModuleIdentity {
                srcversion: Some("S1".into()),
                path: None,
                sha256: None,
            },
            build_info: BTreeMap::new(),
            tsc_frequency: 2_995_200_000,
            boot_seed: 1,
        };
        assert!(env.diff(&env.clone()).is_empty());
        let mut now = env.clone();
        now.initrd.sha256 = "B".into();
        now.bedrock_ko.srcversion = Some("S2".into());
        now.boot_seed = 2;
        let d = env.diff(&now);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].starts_with("initrd"));
        // Unknown on either side is not a mismatch.
        now = env.clone();
        now.bedrock_ko.srcversion = None;
        assert!(env.diff(&now).is_empty());
    }
}
