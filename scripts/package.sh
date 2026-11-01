#!/usr/bin/env bash
# Build the Raspberry Pi bundles from Linux (or WSL) with cargo-zigbuild:
#   dist/read5-uploader-<version>-<commit>-arm64.tar.gz  (64-bit Pi OS: Pi 3, 4, 5, Zero 2)
#   dist/read5-uploader-<version>-<commit>-armhf.tar.gz  (32-bit Pi OS: Pi 2, 3, 4)
# Each holds the binary, the systemd unit, the example config, install.sh,
# the README and the licences. Needs glibc 2.31 (Pi OS Bullseye) or later.
#
# Needs: rustup targets aarch64-unknown-linux-gnu and armv7-unknown-linux-gnueabihf,
# `cargo install cargo-zigbuild`, zig on PATH (`pip install ziglang` works),
# and cmake (libopus is built from source and linked statically).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
OUT=${OUT:-$ROOT/dist}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
GIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)
mkdir -p "$OUT"

for pair in aarch64-unknown-linux-gnu:arm64 armv7-unknown-linux-gnueabihf:armhf; do
  triple=${pair%%:*}
  arch=${pair##*:}
  (cd "$ROOT" && CARGO_TARGET_DIR=$TARGET cargo zigbuild --release -q --target "$triple.2.31")
  name=read5-uploader-$VERSION-$GIT-$arch
  stage=$(mktemp -d)
  mkdir "$stage/$name"
  cp "$TARGET/$triple/release/read5-uploader" "$stage/$name/"
  cp "$ROOT/deploy/read5-uploader.service" "$ROOT/deploy/read5-uploader.toml" "$ROOT/deploy/install.sh" \
     "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_LICENSES.md" "$stage/$name/"
  chmod 755 "$stage/$name/install.sh" "$stage/$name/read5-uploader"
  tar -C "$stage" -czf "$OUT/$name.tar.gz" "$name"
  rm -rf "${stage:?}"
  (cd "$OUT" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
  echo "$OUT/$name.tar.gz"
done
