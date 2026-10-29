#!/bin/bash
# Build the Keep binary on this machine, stage it, and build the one-image Keep + Proton
# tunnel (Dockerfile.proton). Run from anywhere. CARGO_TARGET_DIR is respected if you set it.
# Usage: build-image-proton.sh [IMAGE:TAG]   (default stampede-keep-proton:s1a-2)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
image="${1:-stampede-keep-proton:s1a-2}"

(cd "$root" && cargo build --locked --release -p race-session --bin stampede-headless)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$target/release/stampede-headless" "$stage/"
cp "$here/ingle-entrypoint.sh" "$stage/"
cp "$here/Dockerfile.proton" "$stage/Dockerfile"
docker build --provenance=false --sbom=false -t "$image" "$stage"
