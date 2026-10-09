# Userland tools: bedrock-cli, bedrock-determinism, and bedrock-dst
{ pkgs }:

let
  src = pkgs.lib.cleanSourceWith {
    src = ./..;
    filter = path: type:
      let baseName = builtins.baseNameOf path; in
      # Exclude kernel module build artifacts and non-cargo dirs
      !(baseName == "target" ||
        baseName == ".git" ||
        baseName == ".claude" ||
        baseName == "nix" ||
        # Exclude the kernel module crate (no Cargo.toml, breaks workspace)
        (type == "directory" && baseName == "bedrock" &&
         builtins.match ".*/crates/bedrock$" path != null));
  };
in
{
  bedrock-cli = pkgs.rustPlatform.buildRustPackage {
    pname = "bedrock-cli";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "-p" "bedrock-cli" ];
    meta.mainProgram = "bedrock-cli";
  };

  bedrock-determinism = pkgs.rustPlatform.buildRustPackage {
    pname = "bedrock-determinism";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "-p" "bedrock-determinism-tests" ];
    meta.mainProgram = "bedrock-determinism";
  };

  bedrock-dst = pkgs.rustPlatform.buildRustPackage {
    pname = "bedrock-dst";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "-p" "bedrock-dst" ];
    doCheck = false;
    meta.mainProgram = "bedrock-dst";
  };

  tempo-dst = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = "tempo-dst";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "-p" "tempo-dst" ];
    doCheck = false;
    meta.mainProgram = "tempo-dst";
  };
}
