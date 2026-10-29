#!/usr/bin/env bash
#
# Build the Stampede .deb (amd64/native arch) from the current working tree.
#
#   tools/linux/build-deb.sh
#   SKIP_BUILD=1 tools/linux/build-deb.sh      # reuse the release binary already built
#
# Output (outside the repo): ${OUT:-~/.cache/stampede-linux/out}/stampede_<version>_<arch>.deb
# Env: CARGO_TARGET_DIR (shared ~/.cache/twangthang-target), JOBS, NICE, OUT,
#      MAINTAINER ("Name <email>"), STAMPEDE_LINUX_CACHE.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

command -v dpkg-deb >/dev/null || die "dpkg-deb not found"
DEB_ARCH="$(dpkg --print-architecture)"
MAINTAINER="${MAINTAINER:-decented <noreply@decented.invalid>}"

build_binary
ensure_icons

PKG="$WORK/deb-stage"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$OUT"

log "staging $PKG"
install -Dm755 "$BIN" "$PKG/usr/bin/stampede"
install_common "$PKG"

# Copyright: MIT (repo LICENSE), Axe'n'Stax notice, then the bundled fonts' OFL texts.
DOC="$PKG/usr/share/doc/stampede"
install -d "$DOC"
{
  cat <<HDR
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: Stampede
Comment: The text below is the verbatim LICENSE and third-party notices of the
 source tree (LICENSE, app/NOTICE, app/assets/fonts/*/OFL.txt). The binary also
 statically links Rust crates (see Cargo.lock in the source), all under
 permissive licences (MIT, Apache-2.0, BSD, Zlib, OFL, Unicode, ISC).

Files: *
Copyright: $(sed -n 's/^Copyright (c) //p' "$REPO/LICENSE" | head -1)
License: MIT

License: MIT
HDR
  sed -e '1,/^$/d' -e 's/^$/./' -e 's/^/ /' "$REPO/LICENSE"
  echo
  echo "Files: app/src/app.rs app/src/lib.rs app/src/engine.rs app/src/render/gpu.rs"
  echo " app/src/render/capture.rs app/src/ui/painter.rs app/src/platform/input.rs"
  echo "Copyright: Axe'n'Stax authors (patterns adapted, not copied verbatim)"
  echo "License: MIT"
  echo " See the MIT text above; upstream commit bdd4fe877acf3a6f4ca3174d1ce79c6672935e2f."
  echo
  echo "Files: app/assets/fonts/lilita-one/* app/assets/fonts/exo2/*"
  echo "Copyright: 2011 Juan Montoreano (Lilita One); 2013 The Exo 2 Project Authors"
  echo "License: OFL-1.1"
  echo " Lilita One and Exo 2 are embedded in the binary under the SIL Open Font"
  echo " License 1.1; the full text of each follows."
  for f in lilita-one exo2; do
    echo " ."
    echo " ---- $f ----"
    sed -e 's/[[:space:]]*$//' -e 's/^$/./' -e 's/^/ /' "$REPO/app/assets/fonts/$f/OFL.txt"
  done
} > "$DOC/copyright"
chmod 644 "$DOC/copyright"

# Native-package changelog (lintian wants one); the real history is in git.
{
  echo "stampede ($VERSION) unstable; urgency=low"
  echo
  echo "  * Release $VERSION (built from $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown))."
  echo
  echo " -- $MAINTAINER  $(date -R -d "@$SOURCE_DATE_EPOCH")"
} | gzip -9n > "$DOC/changelog.gz"
chmod 644 "$DOC/changelog.gz"

# Depends: the linked libs (libc6, libgcc-s1, libasound2) from dpkg-shlibdeps,
# plus what winit/wgpu/rodio dlopen at run time, which shlibdeps cannot see.
SHL="$WORK/shlibdeps"
rm -rf "$SHL"; mkdir -p "$SHL/debian"
printf 'Source: stampede\n\nPackage: stampede\nArchitecture: any\n' > "$SHL/debian/control"
SHLIB_DEPS="$(cd "$SHL" && dpkg-shlibdeps -O -e "$PKG/usr/bin/stampede" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
[ -n "$SHLIB_DEPS" ] || die "dpkg-shlibdeps produced nothing"
# Debian 12 calls it libasound2; Ubuntu 24.04 / Mint 22 libasound2t64.
SHLIB_DEPS="$(printf '%s' "$SHLIB_DEPS" | sed -E 's/libasound2t64( \([^)]*\))?/libasound2t64\1 | libasound2\1/')"
RUNTIME_DEPS="libvulkan1 | libegl1, libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1"
DEPENDS="$SHLIB_DEPS, $RUNTIME_DEPS"
RECOMMENDS="mesa-vulkan-drivers | libgl1-mesa-dri, xclip | wl-clipboard"

INSTALLED_KB="$(du -sk --exclude=DEBIAN "$PKG" | cut -f1)"
cat > "$PKG/DEBIAN/control" <<CTL
Package: stampede
Version: $VERSION
Section: games
Priority: optional
Architecture: $DEB_ARCH
Maintainer: $MAINTAINER
Installed-Size: $INSTALLED_KB
Depends: $DEPENDS
Recommends: $RECOMMENDS
Description: Laser-eyed ostrich racing
 Stampede is a Mario Kart-style arcade racer starring laser-eyed ostriches.
 Race solo against bots, fight in battle arenas, or race friends on the
 local network. Native Rust, no game engine; needs Vulkan or OpenGL ES.
 .
 If frames stall for about a second each on Intel graphics under X11, start
 it with STAMPEDE_NO_VSYNC=1 (the menu entry has a "vsync off" action).
CTL

# Normalise modes (umask-independent) and timestamps, then pack.
find "$PKG" -type d -exec chmod 755 {} +
find "$PKG" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
DEB="$OUT/stampede_${VERSION}_${DEB_ARCH}.deb"
log "dpkg-deb -> $DEB"
dpkg-deb --root-owner-group -Zxz --build "$PKG" "$DEB"
validate_metadata "$PKG"
if command -v lintian >/dev/null; then lintian "$DEB" || true; else echo "(lintian not installed; skipped)"; fi
ls -l "$DEB"
