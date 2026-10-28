#!/bin/sh
# Lints, tests and builds the plugin for Linux in a Debian 13 container (needs Docker); the program needs a system
# with that glibc or newer. The Rust version comes from rust-toolchain.toml.
#   sh ci/linux.sh                        host architecture (aarch64 on Apple Silicon)
#   sh ci/linux.sh --platform linux/amd64 x86_64, emulated on Apple Silicon (slow)
# With PACKAGE=1 it also packs dist/opendeck-masterhub-linux-<arch>.zip (VERSION sets the manifest version).
set -e
cd "$(dirname "$0")/.."
command -v docker >/dev/null || { echo "Docker is needed for the Linux build"; exit 1; }
# always an explicit platform: the local rust image's tag holds whichever architecture was pulled last
PLATFORM="--platform linux/$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')"
[ "$1" = "--platform" ] && PLATFORM="--platform $2"
mkdir -p dist
# the source is mounted read-only and copied, so the host's target/ stays the host's
docker run --rm $PLATFORM -e PACKAGE -e VERSION -v "$PWD":/src:ro -v "$PWD/dist":/dist rust:1-trixie bash -ec '
  apt-get update -qq && apt-get install -y -qq libudev-dev pkg-config zip >/dev/null
  cp -r /src /work && cd /work && rm -rf target dist pl.kobylarek.masterhub.sdPlugin/bin
  rustup component add clippy >/dev/null
  cargo clippy --locked --all-targets -- -D warnings
  cargo test --locked
  cargo build --release --locked
  if [ -n "$PACKAGE" ]; then
    triple=$(rustc -vV | sed -n "s/^host: //p")
    mkdir -p pl.kobylarek.masterhub.sdPlugin/bin/$triple
    cp target/release/opendeck-masterhub pl.kobylarek.masterhub.sdPlugin/bin/$triple/
    sh ci/package.sh linux-${triple%%-*}
    cp dist/*.zip /dist/
  fi'
