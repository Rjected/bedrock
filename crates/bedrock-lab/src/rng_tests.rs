// SPDX-License-Identifier: GPL-2.0

//! [`InputRecording::record_event`] tests over hand-built event bytes (no VM).

use super::{
    Cut, InputRecording, InputSource, IoInput, PrefixSource, RandomInput, RecordedInputSource,
    SeededSource,
};
use crate::bash::BashTarget;
use crate::time::VirtTime;
use bedrock_vm::events::{
    EventKind, IoChannelPayload, IoChannelPhase, RandomPayload, RandomSource,
    EVENT_FLAG_DETERMINISTIC, EVENT_HEADER_SIZE,
};
use bedrock_vm::io_channel::encode_request;
use bedrock_vm::EventStream;

const FREQ: u64 = 2_995_200_000;

/// Append one TLV record to `buf`, padding to an 8-byte boundary — mirrors the
/// kernel producer's framing (see `bedrock-vm/src/events_tests.rs`).
fn push_record(buf: &mut Vec<u8>, seq: u64, tsc: u64, kind: u16, payload: &[u8]) {
    let before = buf.len();
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&tsc.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes()); // real_tsc — ignored on decode
    buf.extend_from_slice(&kind.to_le_bytes());
    buf.extend_from_slice(&EVENT_FLAG_DETERMINISTIC.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    assert_eq!(buf.len() - before, EVENT_HEADER_SIZE);
    buf.extend_from_slice(payload);
    while !buf.len().is_multiple_of(8) {
        buf.push(0);
    }
}

fn random_record(buf: &mut Vec<u8>, seq: u64, tsc: u64, value: u64) {
    let p = RandomPayload {
        value,
        source: 0, // RDRAND
        width: 8,
        ..RandomPayload::default()
    };
    push_record(buf, seq, tsc, EventKind::Randomness.as_u16(), p.as_bytes());
}

/// A `source = GetRandom` randomness record with trailing served bytes.
fn get_random_record(buf: &mut Vec<u8>, seq: u64, tsc: u64, pid: u32, bytes: &[u8]) {
    let p = RandomPayload {
        pid,
        source: RandomSource::GetRandom as u8,
        ..RandomPayload::default()
    };
    let mut payload = p.as_bytes().to_vec();
    payload.extend_from_slice(bytes);
    push_record(buf, seq, tsc, EventKind::Randomness.as_u16(), &payload);
}

fn io_record(buf: &mut Vec<u8>, seq: u64, tsc: u64, phase: IoChannelPhase, envelope: &[u8]) {
    let meta = IoChannelPayload {
        phase: phase as u8,
        _pad: [0; 7],
        // Source-driven requests are queued fire-ASAP, so the wire `target_tsc`
        // is 0; the header `tsc` is what the recording uses for `at`.
        target_tsc: 0,
    };
    let mut payload = meta.as_bytes().to_vec();
    payload.extend_from_slice(envelope);
    push_record(buf, seq, tsc, EventKind::IoChannel.as_u16(), &payload);
}

/// Drive every record in `buf` through `record_event`, as `drain_events` does.
fn recording_from(buf: &[u8]) -> InputRecording {
    let mut rec = InputRecording::new();
    for record in EventStream::new(buf) {
        rec.record_event(&record, FREQ);
    }
    rec
}

#[test]
fn rdrand_events_become_random_inputs() {
    let mut buf = Vec::new();
    random_record(&mut buf, 0, 1_000, 0xDEAD_BEEF);
    random_record(&mut buf, 1, 2_000, 0x0BAD_F00D);

    let rec = recording_from(&buf);
    assert_eq!(rec.io_inputs().len(), 0);
    // RDRAND/RDSEED land in the one randomness stream, value stored as bytes.
    assert_eq!(
        rec.random_inputs(),
        &[
            RandomInput {
                at: VirtTime::from_instructions(1_000, FREQ),
                source: RandomSource::Rdrand,
                pid: 0,
                bytes: 0xDEAD_BEEF_u64.to_le_bytes().to_vec(),
            },
            RandomInput {
                at: VirtTime::from_instructions(2_000, FREQ),
                source: RandomSource::Rdrand,
                pid: 0,
                bytes: 0x0BAD_F00D_u64.to_le_bytes().to_vec(),
            },
        ]
    );

    // Replays as instruction values via next_rng_u64.
    let mut src = RecordedInputSource::new(rec);
    assert_eq!(src.next_rng_u64(), Some(0xDEAD_BEEF));
    assert_eq!(src.next_rng_u64(), Some(0x0BAD_F00D));
    assert_eq!(src.next_rng_u64(), None);
}

#[test]
fn get_random_events_become_random_inputs() {
    let mut buf = Vec::new();
    get_random_record(&mut buf, 0, 1_000, 42, &[1, 2, 3, 4]);
    get_random_record(&mut buf, 1, 2_000, 7, &[0xAA; 16]);

    let rec = recording_from(&buf);
    // GET_RANDOM lands in the same stream as RDRAND, tagged by source.
    assert_eq!(rec.io_inputs().len(), 0);
    assert_eq!(
        rec.random_inputs(),
        &[
            RandomInput {
                at: VirtTime::from_instructions(1_000, FREQ),
                source: RandomSource::GetRandom,
                pid: 42,
                bytes: vec![1, 2, 3, 4],
            },
            RandomInput {
                at: VirtTime::from_instructions(2_000, FREQ),
                source: RandomSource::GetRandom,
                pid: 7,
                bytes: vec![0xAA; 16],
            },
        ]
    );

    // Replays in order via next_random, and fails (never zero-fills) past the
    // recording.
    let mut src = RecordedInputSource::new(rec);
    assert_eq!(src.next_random(4, 42), Some(vec![1, 2, 3, 4]));
    assert_eq!(src.next_random(16, 7), Some(vec![0xAA; 16]));
    assert_eq!(src.exhaustion(), None);
    assert_eq!(src.next_random(3, 0), None);
    let why = src.exhaustion().unwrap();
    assert!(why.contains("exhausted"), "{why}");
    assert!(why.contains("holds only 2"), "{why}");
    assert_eq!(src.random_consumed(), 2);
}

#[test]
fn recorded_source_fails_on_exhaustion_and_divergence() {
    let mut buf = Vec::new();
    get_random_record(&mut buf, 0, 1_000, 42, &[1, 2, 3, 4]);
    random_record(&mut buf, 1, 2_000, 0xAB);
    let rec = recording_from(&buf);

    // A GET_RANDOM of another length than recorded diverged: no bytes, and
    // the cursor stays put.
    let mut src = RecordedInputSource::new(rec.clone());
    assert_eq!(src.next_random(8, 42), None);
    let why = src.exhaustion().unwrap();
    assert!(why.contains("diverged at randomness input #0"), "{why}");
    assert!(why.contains("GetRandom of 8 bytes"), "{why}");
    assert_eq!(src.random_consumed(), 0);
    // Once failed, it keeps failing (InputSource contract).
    assert_eq!(src.next_random(4, 42), None);
    assert_eq!(src.next_rng_u64(), None);

    // An RDRAND where the tape has GET_RANDOM diverged too.
    let mut src = RecordedInputSource::new(rec.clone());
    assert_eq!(src.next_rng_u64(), None);
    assert!(src.exhaustion().unwrap().contains("RDRAND"));

    // In order it serves everything, then reports exhaustion.
    let mut src = RecordedInputSource::new(rec);
    assert_eq!(src.next_random(4, 42), Some(vec![1, 2, 3, 4]));
    assert_eq!(src.next_rng_u64(), Some(0xAB));
    assert_eq!(src.next_rng_u64(), None);
    assert!(src.exhaustion().unwrap().contains("input tape exhausted"));
}

#[test]
fn default_next_random_fails_when_the_source_runs_dry() {
    let mut left = 1u32;
    let mut src = move || {
        left = left.checked_sub(1)?;
        Some(0x0102_0304_0506_0708_u64)
    };
    assert_eq!(
        src.next_random(8, 0),
        Some(0x0102_0304_0506_0708_u64.to_le_bytes().to_vec())
    );
    assert_eq!(src.next_random(1, 0), None);
}

#[test]
fn rdrand_and_get_random_share_one_ordered_stream() {
    // RDRAND and GET_RANDOM share one stream and replay off one cursor.
    let mut buf = Vec::new();
    random_record(&mut buf, 0, 1_000, 0xAB); // RDRAND value
    get_random_record(&mut buf, 1, 2_000, 9, &[7, 7, 7, 7]); // GET_RANDOM bytes
    random_record(&mut buf, 2, 3_000, 0xCD); // RDRAND value

    let rec = recording_from(&buf);
    assert_eq!(rec.random_inputs().len(), 3);

    let mut src = RecordedInputSource::new(rec);
    assert_eq!(src.next_rng_u64(), Some(0xAB));
    assert_eq!(src.next_random(4, 9), Some(vec![7, 7, 7, 7]));
    assert_eq!(src.next_rng_u64(), Some(0xCD));
    assert_eq!(src.next_rng_u64(), None);
}

#[test]
fn io_request_events_become_io_inputs() {
    let mut buf = Vec::new();
    io_record(
        &mut buf,
        0,
        3_000,
        IoChannelPhase::Request,
        &encode_request(None, "echo hi", true),
    );
    io_record(
        &mut buf,
        1,
        4_000,
        IoChannelPhase::Request,
        &encode_request(Some("bitcoind1"), "bitcoin-cli getinfo", false),
    );

    let rec = recording_from(&buf);
    assert_eq!(rec.random_inputs().len(), 0);
    assert_eq!(
        rec.io_inputs(),
        &[
            IoInput {
                at: VirtTime::from_instructions(3_000, FREQ),
                target: BashTarget::Host,
                command: "echo hi".to_string(),
                record_output: true,
            },
            IoInput {
                at: VirtTime::from_instructions(4_000, FREQ),
                target: BashTarget::container("bitcoind1"),
                command: "bitcoin-cli getinfo".to_string(),
                record_output: false,
            },
        ]
    );
}

#[test]
fn responses_and_other_kinds_are_ignored() {
    let mut buf = Vec::new();
    // An I/O channel *response* carries host-derived output, not an input.
    io_record(
        &mut buf,
        0,
        10,
        IoChannelPhase::Response,
        b"opaque response",
    );
    // A serial line is not an input either.
    push_record(&mut buf, 1, 20, EventKind::Serial.as_u16(), b"hello\n");
    // ...but a request between them still records.
    io_record(
        &mut buf,
        2,
        30,
        IoChannelPhase::Request,
        &encode_request(None, "true", false),
    );

    let rec = recording_from(&buf);
    assert_eq!(rec.random_inputs().len(), 0);
    assert_eq!(rec.io_inputs().len(), 1);
    assert_eq!(rec.io_inputs()[0].command, "true");
}

#[test]
fn recording_round_trips_through_replay_source() {
    let mut buf = Vec::new();
    random_record(&mut buf, 0, 1_000, 0x11);
    io_record(
        &mut buf,
        1,
        2_000,
        IoChannelPhase::Request,
        &encode_request(None, "first", false),
    );
    random_record(&mut buf, 2, 3_000, 0x22);
    io_record(
        &mut buf,
        3,
        4_000,
        IoChannelPhase::Request,
        &encode_request(None, "second", true),
    );

    let rec = recording_from(&buf);
    let mut source = RecordedInputSource::new(rec);

    // Randomness and I/O each replay in capture order, on independent cursors.
    assert_eq!(source.next_rng_u64(), Some(0x11));
    assert_eq!(source.next_rng_u64(), Some(0x22));
    assert_eq!(source.next_rng_u64(), None);

    let first = source.next_io_input().unwrap();
    assert_eq!(first.command, "first");
    assert!(!first.record_output);
    let second = source.next_io_input().unwrap();
    assert_eq!(second.command, "second");
    assert!(second.record_output);
    assert!(source.next_io_input().is_none());
}

#[test]
fn seeded_source_serves_the_kernel_stream() {
    use bedrock_vmx::devices::RandomState;
    for seed in [0u64, 1, 7, 0xbed0_7e3b] {
        let mut kernel = RandomState::seeded_rng(seed);
        let mut src = super::SeededSource::new(seed);
        // RDRAND: one stream value each.
        for _ in 0..4 {
            assert_eq!(src.next_rng_u64(), Some(kernel.next_seeded_u64()));
        }
        // GET_RANDOM: the in-kernel fill (vmcall.rs) takes one value per 8
        // bytes and truncates the last.
        for len in [8usize, 12, 1, 256] {
            let mut want = Vec::new();
            while want.len() < len {
                let n = (len - want.len()).min(8);
                want.extend_from_slice(&kernel.next_seeded_u64().to_le_bytes()[..n]);
            }
            assert_eq!(src.next_random(len, 0), Some(want));
        }
    }
}

fn t(instructions: u64) -> VirtTime {
    VirtTime::from_instructions(instructions, FREQ)
}

fn rdrand_at(at: u64, value: u64) -> RandomInput {
    RandomInput {
        at: t(at),
        source: RandomSource::Rdrand,
        pid: 0,
        bytes: value.to_le_bytes().to_vec(),
    }
}

fn io_at(at: u64, command: &str) -> IoInput {
    IoInput {
        at: t(at),
        target: BashTarget::Host,
        command: command.into(),
        record_output: true,
    }
}

/// Four RDRANDs at 1k..4k with a `start` action at 500 and a `finalize` at
/// 5k: the shape of a seed's tape.
fn run_recording() -> InputRecording {
    InputRecording::from_parts(
        vec![
            rdrand_at(1_000, 0x11),
            rdrand_at(2_000, 0x22),
            rdrand_at(3_000, 0x33),
            rdrand_at(4_000, 0x44),
        ],
        vec![io_at(500, "start"), io_at(5_000, "finalize")],
    )
}

/// A fallback that counts up from `base`, standing in for a branch seed.
fn counter(base: u64) -> impl FnMut() -> Option<u64> + Clone + Send + Sync + 'static {
    let mut n = base;
    move || {
        n += 1;
        Some(n)
    }
}

#[test]
fn prefix_source_switches_at_an_input_index() {
    let rec = run_recording();
    let cut = rec.cut_at_index(2);
    // Host actions before input #2's time go with the prefix.
    assert_eq!(cut, Cut { random: 2, io: 1 });
    let mut src = PrefixSource::new(&rec, cut, counter(100));
    assert!(src.in_prefix());
    assert_eq!(src.next_rng_u64(), Some(0x11));
    assert_eq!(src.next_rng_u64(), Some(0x22));
    assert!(!src.in_prefix());
    assert_eq!(src.prefix_consumed(), 2);
    // Then the fallback, from its own start.
    assert_eq!(src.next_rng_u64(), Some(101));
    assert_eq!(src.next_random(8, 7), Some(102u64.to_le_bytes().to_vec()));
    // Host actions: the prefix's, then the fallback's (none).
    assert_eq!(src.next_io_input().unwrap().command, "start");
    assert!(src.next_io_input().is_none());
    // Past the end: everything up to the last input is prefix.
    assert_eq!(rec.cut_at_index(99), Cut { random: 4, io: 1 });
}

#[test]
fn prefix_source_switches_at_a_virtual_time() {
    let rec = run_recording();
    // Strictly before t: an input at exactly 3_000 is not in the prefix.
    let cut = rec.cut_at_time(t(3_000));
    assert_eq!(cut, Cut { random: 2, io: 1 });
    assert_eq!(rec.cut_at_time(t(3_001)), Cut { random: 3, io: 1 });
    assert_eq!(rec.cut_at_time(t(0)), Cut::default());
    assert_eq!(rec.cut_at_time(t(9_000)), Cut { random: 4, io: 2 });
    let mut src = PrefixSource::new(&rec, rec.cut_at_time(t(2_500)), counter(0));
    assert_eq!(src.cut(), Cut { random: 2, io: 1 });
    let served: Vec<_> = (0..4).map(|_| src.next_rng_u64().unwrap()).collect();
    assert_eq!(served, [0x11, 0x22, 1, 2]);
}

#[test]
fn recording_is_prefix_plus_suffix() {
    let rec = run_recording();
    for n in 0..=4 {
        let cut = rec.cut_at_index(n);
        let (prefix, suffix) = (rec.prefix(cut), rec.suffix(cut));
        assert_eq!(prefix.random_inputs().len(), n);
        assert_eq!(prefix.concat(&suffix), rec);
        assert!(rec.starts_with(&prefix));

        // The recording's own suffix as fallback serves the recording exactly
        // (a branch that varies nothing is the original run).
        let mut src = PrefixSource::new(&rec, cut, RecordedInputSource::new(suffix.clone()));
        let served: Vec<_> = (0..4).map(|_| src.next_rng_u64().unwrap()).collect();
        assert_eq!(served, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(src.next_rng_u64(), None);
        let io: Vec<_> = std::iter::from_fn(|| src.next_io_input())
            .map(|i| i.command)
            .collect();
        assert_eq!(io, ["start", "finalize"]);

        // A branch forked at the cut has consumed the prefix already.
        let mut after =
            PrefixSource::new(&rec, cut, RecordedInputSource::new(suffix)).after_prefix();
        assert!(!after.in_prefix());
        let rest: Vec<_> = std::iter::from_fn(|| after.next_rng_u64()).collect();
        assert_eq!(rest, [0x11, 0x22, 0x33, 0x44][n..]);
    }
    let other = InputRecording::from_parts(vec![rdrand_at(1_000, 0x99)], vec![]);
    assert!(!rec.starts_with(&other));
}

#[test]
fn prefix_source_is_strict_before_the_cut() {
    let rec = run_recording();
    let mut src = PrefixSource::new(&rec, rec.cut_at_index(2), counter(0));
    // A GET_RANDOM where the prefix recorded an RDRAND: the branch diverged
    // before its moment, so the source stops (and does not fall through).
    assert_eq!(src.next_random(16, 1), None);
    let why = src.exhaustion().unwrap();
    assert!(
        why.contains("recorded prefix") && why.contains("#0"),
        "{why}"
    );
    assert_eq!(src.next_rng_u64(), None);
    // Clones are independent.
    let fresh = PrefixSource::new(&rec, rec.cut_at_index(2), counter(0));
    let mut a = fresh.clone();
    let mut b = fresh.clone_box();
    assert_eq!(a.next_rng_u64(), Some(0x11));
    assert_eq!(b.next_rng_u64(), Some(0x11));
    assert_eq!(a.next_rng_u64(), Some(0x22));
    assert_eq!(a.next_rng_u64(), Some(1));
    assert_eq!(b.next_rng_u64(), Some(0x22));
}

#[test]
fn seeded_source_continues_a_recorded_stream() {
    let mut whole = SeededSource::new(42);
    let mut consumed = Vec::new();
    for (i, len) in [None, Some(12), Some(1), None, Some(256)]
        .into_iter()
        .enumerate()
    {
        let (source, bytes) = match len {
            None => (
                RandomSource::Rdrand,
                whole.next_rng_u64().unwrap().to_le_bytes().to_vec(),
            ),
            Some(n) => (RandomSource::GetRandom, whole.next_random(n, 3).unwrap()),
        };
        consumed.push(RandomInput {
            at: t(i as u64),
            source,
            pid: 3,
            bytes,
        });
    }
    assert_eq!(SeededSource::draws(&consumed), 1 + 2 + 1 + 1 + 32);
    let mut cont = SeededSource::new(42).skip(SeededSource::draws(&consumed));
    for _ in 0..8 {
        assert_eq!(cont.next_rng_u64(), whole.next_rng_u64());
    }
}
