# Shared helpers for build-deb.sh and build-appimage.sh (sourced, not run).

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LINUX_DIR="$REPO/tools/linux"

# Everything generated lives outside the repo.
WORK="${STAMPEDE_LINUX_CACHE:-$HOME/.cache/stampede-linux}"
OUT="${OUT:-$WORK/out}"
# Shared with Twang Thang on purpose (disk is tight): never a fresh target dir.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/twangthang-target}"
JOBS="${JOBS:-4}"
NICE="${NICE:-10}"

APP_ID=org.decented.stampede
DESKTOP_FILE="$APP_ID.desktop"
METAINFO_FILE="$APP_ID.metainfo.xml"
ICON_SIZES="48 64 128 256 512"

log() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Version from [workspace.package] in the root Cargo.toml.
VERSION="$(awk '/^\[workspace.package\]/{f=1;next} /^\[/{f=0} f && /^version[[:space:]]*=/{gsub(/[" ]/,"",$3); print $3; exit}' "$REPO/Cargo.toml")"
[ -n "$VERSION" ] || die "could not read the version from $REPO/Cargo.toml"

# Reproducible timestamps and the metainfo release date: the HEAD commit's.
if SOURCE_DATE_EPOCH="$(git -C "$REPO" log -1 --format=%ct 2>/dev/null)" && [ -n "$SOURCE_DATE_EPOCH" ]; then :; else SOURCE_DATE_EPOCH="$(date +%s)"; fi
export SOURCE_DATE_EPOCH
RELEASE_DATE="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%d)"

ARCH_UNAME="$(uname -m)"   # x86_64 / aarch64
[ "$ARCH_UNAME" = x86_64 ] || echo "warning: only x86_64 has been exercised ($ARCH_UNAME)" >&2

check_disk() {
  local free_kb
  free_kb="$(df -Pk "$HOME" | awk 'NR==2{print $4}')"
  [ "$free_kb" -gt 1500000 ] || echo "warning: under 1.5 GB free in \$HOME ($((free_kb / 1024)) MB); the build may fail" >&2
}

# Release build of the desktop binary into the shared target dir. cargo waits
# on the target-dir lock if another build holds it. SKIP_BUILD=1 reuses what is
# there (the binary must be a release build of this tree).
build_binary() {
  BIN="$CARGO_TARGET_DIR/release/stampede"
  if [ -n "${SKIP_BUILD:-}" ]; then
    [ -x "$BIN" ] || die "SKIP_BUILD set but $BIN does not exist"
    log "reusing $BIN"
    return
  fi
  check_disk
  log "cargo build --release (target dir $CARGO_TARGET_DIR; waits if it is locked)"
  (cd "$REPO" && nice -n "$NICE" cargo build --release --locked -p stampede --bin stampede -j "$JOBS")
  [ -x "$BIN" ] || die "build produced no $BIN"
}

# hicolor PNGs (cached; re-rendered when the artwork scripts change).
ensure_icons() {
  local key
  key="$(cat "$REPO"/tools/icon/*.py "$LINUX_DIR/render-icons.py" | sha256sum | cut -c1-16)"
  ICON_DIR="$WORK/icons-$key"
  if [ ! -f "$ICON_DIR/512x512/stampede.png" ]; then
    log "rendering icons ($ICON_SIZES)"
    rm -rf "$WORK"/icons-*
    # shellcheck disable=SC2086
    python3 "$LINUX_DIR/render-icons.py" "$ICON_DIR" $ICON_SIZES
  fi
}

# install_common DEST: icons + desktop file + metainfo under DEST/usr/share.
install_common() {
  local dest="$1" n
  install -Dm644 "$LINUX_DIR/data/$DESKTOP_FILE" "$dest/usr/share/applications/$DESKTOP_FILE"
  mkdir -p "$dest/usr/share/metainfo"
  sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$RELEASE_DATE/g" \
    "$LINUX_DIR/data/$METAINFO_FILE" > "$dest/usr/share/metainfo/$METAINFO_FILE"
  chmod 644 "$dest/usr/share/metainfo/$METAINFO_FILE"
  for n in $ICON_SIZES; do
    install -Dm644 "$ICON_DIR/${n}x${n}/stampede.png" "$dest/usr/share/icons/hicolor/${n}x${n}/apps/stampede.png"
  done
}

# Validate what we can, without failing the build.
validate_metadata() {
  local dest="$1"
  command -v desktop-file-validate >/dev/null && desktop-file-validate "$dest/usr/share/applications/$DESKTOP_FILE" || true
  command -v appstreamcli >/dev/null && appstreamcli validate --no-net "$dest/usr/share/metainfo/$METAINFO_FILE" || true
}
