//! `HYPERCALL_FILE_FETCH`: the workload files the initrd downloads at boot must
//! hash identically to the host originals.
//!
//! The init script deletes `/images/images.tar` once `podman load` succeeds (it
//! would otherwise occupy guest RAM for the life of the VM) and keeps its
//! digest in `/images/images.tar.sha256`, so that is what is compared.

use bedrock_lab::BashTarget;

use crate::common;

/// Where the guest keeps a downloaded file's digest.
enum Guest {
    /// The file itself; hashed in the guest.
    File(&'static str),
    /// A file holding the sha256 the guest computed before deleting it.
    Digest(&'static str),
}

/// `(host_original, guest)` pairs; the host paths are guaranteed set
/// whenever `ready_checkpoint` succeeds.
fn workload_files() -> [(String, Guest); 2] {
    let compose = std::env::var("BEDROCK_COMPOSE").expect("BEDROCK_COMPOSE set");
    let images = std::env::var("BEDROCK_IMAGES").expect("BEDROCK_IMAGES set");
    [
        (compose, Guest::File("/workload/compose.yaml")),
        (images, Guest::Digest("/images/images.tar.sha256")),
    ]
}

#[test]
fn downloaded_files_match_host_originals() {
    let Some(ready) = common::ready_checkpoint() else {
        return common::skip("downloaded_files_match_host_originals");
    };

    let mut branch = ready.branch().expect("fork branch");

    for (host_path, guest) in workload_files() {
        let want = common::host_sha256(&host_path);
        let (guest_path, got) = match guest {
            Guest::File(path) => (path, common::guest_sha256(&mut branch, path)),
            Guest::Digest(path) => {
                let out = branch
                    .bash(BashTarget::host(), &format!("cat {path}"), true)
                    .expect("dispatch cat in guest");
                assert!(
                    out.success(),
                    "guest `cat {path}` failed — podman load failed? output: {:?}",
                    out.output_lossy(),
                );
                (path, out.output_lossy().trim().to_string())
            }
        };
        assert_eq!(
            got, want,
            "{guest_path} downloaded over the file-transmission hypercall does not \
             match the host original {host_path}",
        );
    }
}
