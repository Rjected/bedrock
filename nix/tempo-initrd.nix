# Tempo's Podman tuning, built once per generic initrd through the Nix store.
{ pkgs, podmanInitrd, amdGlibcBoundary ? false }:

pkgs.runCommand (if amdGlibcBoundary then "amd-tempo-podman-initrd" else "tempo-podman-initrd") {
  nativeBuildInputs = [ pkgs.cpio pkgs.gzip pkgs.python3 pkgs.findutils ];
  BEDROCK_AMD_GLIBC_BOUNDARY = if amdGlibcBoundary then "${pkgs.glibc}/lib/libc.so.6" else "";
  BEDROCK_AMD_GLIBC_PATCH = if amdGlibcBoundary then "${../guest/patches/relocate-glibc-boundary.py}" else "";
} ''
  ${pkgs.bash}/bin/bash ${../workloads/tempo/prepare-initrd.sh} ${podmanInitrd} "$out"
''
