// SPDX-License-Identifier: GPL-2.0

//! Per-writer assertion files, live forwarding, and the seed's ordered merge.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub const DIR: &str = "/bedrock/assertions";
const OFFSETS: &str = "/bedrock/in/assertion-offsets.json";
pub const MERGED: &str = "/bedrock/out/assertions.jsonl";

fn files(dir: &Path) -> io::Result<BTreeMap<String, (u64, std::path::PathBuf)>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl") && entry.file_type()?.is_file() {
            out.insert(name, (entry.metadata()?.len(), entry.path()));
        }
    }
    Ok(out)
}

pub fn snapshot() -> io::Result<()> {
    let offsets: BTreeMap<_, _> = files(Path::new(DIR))?
        .into_iter()
        .map(|(name, (len, _))| (name, len))
        .collect();
    fs::write(OFFSETS, serde_json::to_vec(&offsets)?)
}

fn timestamp(line: &str, fallback: u64) -> u64 {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| {
            v.get("Always")
                .or_else(|| v.get("Sometimes"))
                .and_then(|data| data.get("timestamp_unix_nano"))
                .and_then(Value::as_u64)
        })
        .filter(|t| *t > 0)
        .unwrap_or(fallback)
}

fn unix_nano(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn ordered(lines: Vec<(String, usize, String, u64)>) -> String {
    let mut lines = lines;
    lines.sort_by(|a, b| (a.3, &a.0, a.1).cmp(&(b.3, &b.0, b.1)));
    lines
        .into_iter()
        .map(|(_, _, line, _)| format!("{line}\n"))
        .collect()
}

fn merge(dir: &Path, offsets: &BTreeMap<String, u64>) -> io::Result<String> {
    let mut lines = Vec::new();
    for (name, (len, path)) in files(dir)? {
        let offset = offsets
            .get(&name)
            .copied()
            .filter(|n| *n <= len)
            .unwrap_or(0);
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let data = String::from_utf8_lossy(&bytes);
        let fallback = unix_nano(file.metadata()?.modified()?);
        for (index, line) in data.lines().enumerate() {
            lines.push((
                name.clone(),
                index,
                line.to_owned(),
                timestamp(line, fallback),
            ));
        }
    }
    Ok(ordered(lines))
}

pub fn merge_seed() -> io::Result<()> {
    let offsets = serde_json::from_slice(&fs::read(OFFSETS)?)?;
    fs::write(MERGED, merge(Path::new(DIR), &offsets)?)?;
    // The live forwarder holds lines briefly to interleave nearby timestamps.
    // Let it drain finalizer records before the host stops this branch.
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    pending: Vec<u8>,
    index: usize,
}

fn read_new(
    dir: &Path,
    cursors: &mut BTreeMap<String, Cursor>,
) -> io::Result<Vec<(String, usize, String, u64)>> {
    let mut lines = Vec::new();
    for (name, (len, path)) in files(dir)? {
        let cursor = cursors.entry(name.clone()).or_default();
        if len < cursor.offset {
            *cursor = Cursor::default();
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(cursor.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        cursor.offset += bytes.len() as u64;
        cursor.pending.extend(bytes);
        while let Some(end) = cursor.pending.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&cursor.pending[..end]).into_owned();
            cursor.pending.drain(..=end);
            lines.push((
                name.clone(),
                cursor.index,
                line.clone(),
                timestamp(&line, unix_nano(SystemTime::now())),
            ));
            cursor.index += 1;
        }
    }
    Ok(lines)
}

pub fn forward() -> ! {
    let mut cursors = BTreeMap::new();
    let mut pending = Vec::new();
    loop {
        match read_new(Path::new(DIR), &mut cursors) {
            Ok(lines) => pending.extend(lines),
            Err(e) => eprintln!("assertion forwarder: {e}"),
        }
        // Give other writers half a second to publish nearby timestamps.
        let cutoff = unix_nano(SystemTime::now()).saturating_sub(500_000_000);
        let (ready, later) = pending.into_iter().partition(|line| line.3 <= cutoff);
        pending = later;
        let output = ordered(ready);
        if !output.is_empty() {
            let _ = io::stdout().write_all(output.as_bytes());
            let _ = io::stdout().flush();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_only_seed_records_in_timestamp_order() {
        let dir = std::env::temp_dir().join(format!(
            "bedrock-assertions-{}-{}",
            std::process::id(),
            unix_nano(SystemTime::now())
        ));
        fs::create_dir(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        fs::write(&a, "warmup\n").unwrap();
        let offsets = BTreeMap::from([("a.jsonl".to_owned(), 7)]);
        fs::write(&a, "warmup\n{\"Always\":{\"timestamp_unix_nano\":30}}\n").unwrap();
        fs::write(&b, "{\"Sometimes\":{\"timestamp_unix_nano\":20}}\n").unwrap();
        let merged = merge(&dir, &offsets).unwrap();
        assert_eq!(
            merged,
            concat!(
                "{\"Sometimes\":{\"timestamp_unix_nano\":20}}\n",
                "{\"Always\":{\"timestamp_unix_nano\":30}}\n"
            )
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forwarder_discovers_files_and_waits_for_complete_lines() {
        let dir = std::env::temp_dir().join(format!(
            "bedrock-forwarder-{}-{}",
            std::process::id(),
            unix_nano(SystemTime::now())
        ));
        fs::create_dir(&dir).unwrap();
        let mut cursors = BTreeMap::new();
        assert!(read_new(&dir, &mut cursors).unwrap().is_empty());
        let path = dir.join("tempo.jsonl");
        fs::write(&path, b"{\"Always\":{\"timestamp_unix_nano\":2}}").unwrap();
        assert!(read_new(&dir, &mut cursors).unwrap().is_empty());
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(read_new(&dir, &mut cursors).unwrap()[0].3, 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
