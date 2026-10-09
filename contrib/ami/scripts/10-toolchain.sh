#!/usr/bin/env bash
# Toolchain for building Linux 6.18 with Rust and out-of-tree Rust modules
# (bedrock.ko). Versions match the bedrock flake: Rust 1.94.0, bindgen 0.72.1.
# LLVM comes from Debian (19); kernel 6.18 needs >= 15.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

RUSTUP_VERSION=1.28.2
RUSTUP_SHA256=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
RUST_VERSION=1.94.0
BINDGEN_VERSION=0.72.1
LLVM_VERSION=19

apt-get update
apt-get install -y --no-install-recommends \
  build-essential bc bison flex cpio kmod rsync python3 perl \
  libelf-dev libssl-dev libncurses-dev dwarves zstd xz-utils \
  dpkg-dev debhelper fakeroot \
  "clang-${LLVM_VERSION}" "lld-${LLVM_VERSION}" "llvm-${LLVM_VERSION}" "libclang-${LLVM_VERSION}-dev" \
  ca-certificates curl git jq gnupg msr-tools just \
  docker.io sudo xz-utils

# System-wide Rust under /opt/rust, usable by every user.
export RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo
mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"
tmp=$(mktemp -d)
curl -fsSL -o "$tmp/rustup-init" \
  "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init"
echo "${RUSTUP_SHA256}  $tmp/rustup-init" | sha256sum -c -
chmod +x "$tmp/rustup-init"
"$tmp/rustup-init" -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
  --component rust-src,rustfmt,clippy
rm -rf "$tmp"
"$CARGO_HOME/bin/cargo" install --locked bindgen-cli --version "${BINDGEN_VERSION}"
chmod -R a+rX /opt/rust

# Login shells (and the build scripts below) see the same tools: unsuffixed
# LLVM 19 binaries, so kernel and module builds both use LLVM=1.
cat > /etc/profile.d/bedrock-toolchain.sh <<EOF
export RUSTUP_HOME=/opt/rust/rustup
export CARGO_HOME=/opt/rust/cargo
export PATH=/usr/lib/llvm-${LLVM_VERSION}/bin:/opt/rust/cargo/bin:\$PATH
export LIBCLANG_PATH=/usr/lib/llvm-${LLVM_VERSION}/lib
EOF
chmod 644 /etc/profile.d/bedrock-toolchain.sh

# shellcheck source=/dev/null
. /etc/profile.d/bedrock-toolchain.sh
rustc --version
bindgen --version
clang --version | head -1
ld.lld --version
