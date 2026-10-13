# Tempo's Podman tuning, built once per generic initrd through the Nix store.
{ pkgs, podmanInitrd, amdGlibcBoundary ? false, amdSystemdPlt ? false }:

pkgs.runCommand (if amdGlibcBoundary then "amd-tempo-podman-initrd" else "tempo-podman-initrd") {
  nativeBuildInputs = [ pkgs.cpio pkgs.gzip pkgs.python3 pkgs.findutils ];
  BEDROCK_AMD_GLIBC_BOUNDARY = if amdGlibcBoundary then "${pkgs.glibc}/lib/libc.so.6" else "";
  BEDROCK_AMD_GLIBC_PATCH = if amdGlibcBoundary then "${../guest/patches/relocate-glibc-boundary.py}" else "";
  BEDROCK_AMD_SYSTEMD_PLT = if amdSystemdPlt then "${pkgs.systemd}/lib/systemd/libsystemd-shared-259.so" else "";
  BEDROCK_AMD_SYSTEMD_PATCH = if amdSystemdPlt then "${../guest/patches/relocate-systemd-plt-displacements.py}" else "";
} ''
  ${pkgs.bash}/bin/bash ${../workloads/tempo/prepare-initrd.sh} ${podmanInitrd} "$out"
''
