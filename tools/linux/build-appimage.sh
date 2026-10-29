#!/usr/bin/env bash
#
# Build Stampede-<version>-x86_64.AppImage from the current working tree.
#
#   tools/linux/build-appimage.sh
#   SKIP_BUILD=1 tools/linux/build-appimage.sh   # reuse the release binary already built
#
# Output (outside the repo): ${OUT:-~/.cache/stampede-linux/out}/Stampede-<version>-x86_64.AppImage
# appimagetool is taken from PATH, else downloaded (pinned release, sha256
# checked) into ~/.cache/stampede-linux/tools/.
#
# Only the game binary is bundled. GPU drivers (Vulkan ICDs / Mesa / EGL),
# libvulkan, X11, Wayland, ALSA and libc all come from the host, so the
# AppImage runs on distros with a glibc at least as new as the build machine's.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ "$ARCH_UNAME" = x86_64 ] || die "this script downloads the x86_64 appimagetool only"
TOOLS="$WORK/tools"
AIT_VERSION=1.9.1
AIT_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
AIT_URL="https://github.com/AppImage/appimagetool/releases/download/$AIT_VERSION/appimagetool-x86_64.AppImage"

# Print the command that runs appimagetool.
find_appimagetool() {
  if command -v appimagetool >/dev/null; then command -v appimagetool; return; fi
  local ait="$TOOLS/appimagetool-$AIT_VERSION-x86_64.AppImage"
  if [ ! -x "$ait" ]; then
    log "downloading appimagetool $AIT_VERSION" >&2
    mkdir -p "$TOOLS"
    local dl; dl="$(mktemp -d "$TOOLS/dl.XXXXXX")"
    curl -fL --retry 3 -o "$dl/appimagetool" "$AIT_URL" >&2
    echo "$AIT_SHA256  $dl/appimagetool" | sha256sum -c - >&2 || die "appimagetool checksum mismatch (update AIT_SHA256 if you meant to change the version)"
    chmod +x "$dl/appimagetool"
    mv "$dl/appimagetool" "$ait"
    rmdir "$dl"
  fi
  # Needs FUSE 2 to run directly; without it, unpack once (not into /tmp, which may be noexec).
  if ! "$ait" --version >/dev/null 2>&1; then
    local ex="$TOOLS/appimagetool-$AIT_VERSION-extracted"
    if [ ! -x "$ex/AppRun" ]; then
      rm -rf "$ex" "$TOOLS/squashfs-root"
      (cd "$TOOLS" && "$ait" --appimage-extract >/dev/null && mv squashfs-root "$ex")
    fi
    ait="$ex/AppRun"
  fi
  echo "$ait"
}

AIT="$(find_appimagetool)"
"$AIT" --version 2>&1 | head -1

build_binary
ensure_icons

APPDIR="$WORK/appimage-stage/Stampede.AppDir"
rm -rf "$WORK/appimage-stage"
mkdir -p "$APPDIR" "$OUT"

log "staging $APPDIR"
install -Dm755 "$BIN" "$APPDIR/usr/bin/stampede"
install_common "$APPDIR"
# appimagetool wants the desktop file, icon and AppRun at the top level.
cp "$APPDIR/usr/share/applications/$DESKTOP_FILE" "$APPDIR/$DESKTOP_FILE"
cp "$ICON_DIR/256x256/stampede.png" "$APPDIR/stampede.png"
ln -s stampede.png "$APPDIR/.DirIcon"
cat > "$APPDIR/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/stampede" "$@"
RUN
chmod 755 "$APPDIR/AppRun"
validate_metadata "$APPDIR"

APPIMAGE="$OUT/Stampede-$VERSION-x86_64.AppImage"
log "appimagetool -> $APPIMAGE"
rm -f "$APPIMAGE"
ARCH=x86_64 "$AIT" "$APPDIR" "$APPIMAGE"
chmod +x "$APPIMAGE"
ls -l "$APPIMAGE"
