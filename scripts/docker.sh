#!/usr/bin/env bash
# Build the Docker images from Linux (or WSL):
#   read5-uploader        the uploader on its own (Dockerfile)
#   rtlsdrairband-read5   docker-rtlsdrairband with the uploader added
#                         (docker/rtlsdrairband/Dockerfile)
#
#   bash scripts/docker.sh            this machine's architecture, loaded into
#                                     the local Docker as <image>:dev
#   PUSH=1 bash scripts/docker.sh     amd64, arm64 and arm/v7, pushed to $REPO
#                                     as <image>:<version> and <image>:latest
#
# The binary is cross-compiled with cargo-zigbuild, as scripts/package.sh
# does, and only copied into the images. Needs what package.sh needs, plus
# the x86_64-unknown-linux-gnu target and docker buildx. PUSH=1 also needs
# QEMU for the other architectures (the layer rebuilds rtl_airband):
#   docker run --privileged --rm tonistiigi/binfmt --install arm64,arm
# and a `docker login` to the registry.
#
# To build an image on the machine it is for (a Pi, say) with no emulation
# and no Rust there: here, `STAGE_ONLY=1 ARCHES=arm64 bash scripts/docker.sh`
# only cross-compiles into dist/docker/; copy the repository (dist/ included)
# to the Pi and run `bash scripts/docker.sh` there, where, with no cargo, it
# uses the staged binary.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
REPO=${REPO:-ghcr.io/jimvella}
BASE=${BASE:-ghcr.io/sdr-enthusiasts/docker-rtlsdrairband:latest}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)

if [ -n "${PUSH:-}" ]; then
  ARCHES=${ARCHES:-"amd64 arm64 arm"}
else
  case $(uname -m) in
    x86_64) host=amd64 ;;
    aarch64) host=arm64 ;;
    armv7l) host=arm ;;
    *) echo "unknown machine $(uname -m): set ARCHES" >&2; exit 1 ;;
  esac
  ARCHES=${ARCHES:-$host}
fi

platforms=
for arch in $ARCHES; do
  case $arch in
    amd64) triple=x86_64-unknown-linux-gnu platform=linux/amd64 ;;
    arm64) triple=aarch64-unknown-linux-gnu platform=linux/arm64 ;;
    arm) triple=armv7-unknown-linux-gnueabihf platform=linux/arm/v7 ;;
    *) echo "unknown architecture $arch (amd64, arm64 or arm)" >&2; exit 1 ;;
  esac
  if command -v cargo >/dev/null 2>&1; then
    (cd "$ROOT" && CARGO_TARGET_DIR=$TARGET cargo zigbuild --release -q --target "$triple.2.31")
    mkdir -p "$ROOT/dist/docker/$arch"
    cp "$TARGET/$triple/release/read5-uploader" "$ROOT/dist/docker/$arch/"
  elif [ ! -f "$ROOT/dist/docker/$arch/read5-uploader" ]; then
    echo "no cargo here and no dist/docker/$arch/read5-uploader: stage it elsewhere with STAGE_ONLY=1" >&2
    exit 1
  fi
  platforms=${platforms:+$platforms,}$platform
done

[ -z "${STAGE_ONLY:-}" ] || exit 0

build() { # <image> <dockerfile> [build args]
  local image=$1 file=$2
  shift 2
  if [ -n "${PUSH:-}" ]; then
    docker buildx build "$ROOT" -f "$ROOT/$file" --platform "$platforms" "$@" \
      -t "$REPO/$image:$VERSION" -t "$REPO/$image:latest" --push
    echo "$REPO/$image:$VERSION"
  else
    docker buildx build "$ROOT" -f "$ROOT/$file" --platform "$platforms" "$@" -t "$image:dev" --load
    echo "$image:dev"
  fi
}

build read5-uploader Dockerfile
build rtlsdrairband-read5 docker/rtlsdrairband/Dockerfile --build-arg "BASE=$BASE"
