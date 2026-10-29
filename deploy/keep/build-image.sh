#!/bin/bash
# Build the Keep binary on this machine, stage it, and build the image.
# Run from anywhere. CARGO_TARGET_DIR is respected if you set it.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"

(cd "$root" && cargo build --locked --release -p race-session --bin stampede-headless)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$target/release/stampede-headless" "$stage/"
cp "$here/Dockerfile" "$stage/"
docker build -t stampede-keep:k0 "$stage"
