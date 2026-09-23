//! `HYPERCALL_FILE_FETCH`: the workload files the initrd downloads at boot must
//! hash identically to the host originals.

use crate::common;

/// `(host_original, guest_path)` pairs; the host paths are guaranteed set
/// whenever `ready_checkpoint` succeeds.
fn workload_files() -> [(String, &'static str); 2] {
    let compose = std::env::var("BEDROCK_COMPOSE").expect("BEDROCK_COMPOSE set");
    let images = std::env::var("BEDROCK_IMAGES").expect("BEDROCK_IMAGES set");
    [
        (compose, "/workload/compose.yaml"),
        (images, "/images/images.tar"),
    ]
}

#[test]
fn downloaded_files_match_host_originals() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("downloaded_files_match_host_originals");
    };

    let mut branch = ready.branch().expect("fork branch");

    for (host_path, guest_path) in workload_files() {
        let want = common::host_sha256(&host_path);
        let got = common::guest_sha256(&mut branch, guest_path);
        assert_eq!(
            got, want,
            "{guest_path} downloaded over the file-transmission hypercall does not \
             match the host original {host_path}",
        );
    }
}
