# Tempo's Podman tuning, built once per generic initrd through the Nix store.
{ pkgs, podmanInitrd }:

pkgs.runCommand "tempo-podman-initrd" {
  nativeBuildInputs = [ pkgs.cpio pkgs.gzip pkgs.python3 pkgs.findutils ];
} ''
  ${pkgs.bash}/bin/bash ${../workloads/tempo/prepare-initrd.sh} ${podmanInitrd} "$out"
''
