//! RNG determinism under `RngMode::Seeded`: sibling branches doing the same
//! work see identical randomness, down to `/dev/urandom`.

use bedrock_lab::{BashTarget, EventCategories, EventConfig};

use crate::common;

/// Read a fixed number of hex-encoded bytes from the guest's `/dev/urandom`.
fn read_urandom(ready: &bedrock_lab::Checkpoint) -> Vec<u8> {
    let mut branch = ready.branch().expect("fork branch");
    let out = branch
        .bash(
            BashTarget::host(),
            "head -c 32 /dev/urandom | od -An -tx1",
            true,
        )
        .expect("bash");
    assert!(
        out.success(),
        "urandom read failed: status={} exit={}",
        out.status,
        out.exit_code,
    );
    out.output
}

#[test]
fn seeded_rng_makes_urandom_deterministic() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("seeded_rng_makes_urandom_deterministic");
    };

    let a = read_urandom(&ready);
    let b = read_urandom(&ready);

    assert!(!a.is_empty(), "expected some random bytes to be captured");
    assert_eq!(
        a,
        b,
        "seeded RNG should make guest /dev/urandom byte-identical across \
         sibling branches:\n a={:?}\n b={:?}",
        String::from_utf8_lossy(&a),
        String::from_utf8_lossy(&b),
    );
}

/// The guest kernel's urandom/getrandom patch is in effect: a `/dev/urandom`
/// read surfaces as `GetRandom` randomness records (the seeded CRNG would be
/// deterministic too, but emit none).
#[test]
fn urandom_reads_route_through_get_random_hypercall() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("urandom_reads_route_through_get_random_hypercall");
    };

    let sink = common::capture_sink();
    let mut branch = ready.branch().expect("fork branch");
    branch
        .set_event_config(&EventConfig {
            categories: EventCategories::RANDOMNESS,
            ..Default::default()
        })
        .expect("enable randomness capture");
    let id = branch.id();

    let out = branch
        .bash(
            BashTarget::host(),
            "head -c 64 /dev/urandom | od -An -tx1",
            true,
        )
        .expect("bash");
    assert!(
        out.success(),
        "urandom read failed: status={} exit={}",
        out.status,
        out.exit_code,
    );

    // `RandomSource::GetRandom` serializes as source == 2 in the record body
    // ({"kind":"randomness","data":{"source":2,"len":N,..}}).
    let records = sink.take_deterministic(id);
    let get_random: Vec<_> = records
        .iter()
        .filter(|r| r.get("kind").and_then(|k| k.as_str()) == Some("randomness"))
        .filter(|r| r.pointer("/data/source").and_then(|s| s.as_u64()) == Some(2))
        .collect();

    assert!(
        !get_random.is_empty(),
        "reading /dev/urandom should emit HYPERCALL_GET_RANDOM (source=GetRandom) \
         randomness records — proving the getrandom() VMCALL patch routes the \
         guest CRNG through the hypervisor. Captured records: {records:?}",
    );

    // At least the 64 requested; reads may be chunked and other readers add more.
    let served: u64 = get_random
        .iter()
        .filter_map(|r| r.pointer("/data/len").and_then(|l| l.as_u64()))
        .sum();
    assert!(
        served >= 64,
        "expected at least the 64 requested bytes served via GET_RANDOM, got {served}",
    );
}
