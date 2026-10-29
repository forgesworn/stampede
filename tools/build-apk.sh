#!/usr/bin/env bash
#
# Build the Stampede Android APK without Gradle (adapted from Axe'n'Stax's
# tools/packaging/android/build-apk.sh, MIT).
#
# The Java is small (app/android/java): StampedeActivity (NativeActivity +
# onNewIntent), RoomService, and the in-app updater (Updater, UpdateReceiver,
# ApkCheck: download, verify, PackageInstaller); every other OS service is a
# JNI call from Rust. So the
# pipeline is
#   cargo-ndk -> aapt2 compile/link -> javac + d8 -> zip in the .so and
#   classes.dex -> zipalign -> apksigner.
#
# Usage:
#   tools/build-apk.sh                   # release profile, arm64-v8a
#   PROFILE=debug tools/build-apk.sh     # unoptimised workspace crates (slow on device)
#   ABI=x86_64 tools/build-apk.sh        # emulator
#   DIAG=1 tools/build-apk.sh            # diagnostic: unstripped .so + debuggable
#                                        # manifest, so `adb shell debuggerd -b <pid>`
#                                        # shows symbolised native stacks. Never ship.
#   STAMPEDE_QUIET=1 DIAG=1 tools/build-apk.sh  # silent device checks; keeps saved audio settings
#
# Outputs (outside the repo):
#   ${OUT:-~/.cache/stampede-android/out}/stampede_<version>_<abi>.apk
#   ...and a stable symlink stampede.apk next to it.
#
# Signing: the Stampede release key at ~/.stampede-release/stampede.keystore
# (never committed; back it up). Without it the build falls back to a
# throwaway key at ~/.cache/stampede-android/debug.keystore, generated on first
# use. An APK signed with the throwaway key can NOT be updated in place by a
# build signed with the real key (adb install -r fails with a signature
# mismatch): uninstall first. Only the throwaway key is in use until a release
# key is made.
#
# Environment (all optional): ANDROID_HOME (default ~/Android/Sdk),
# ANDROID_NDK_HOME (default: newest NDK), BUILD_TOOLS, PLATFORM, ABI, PROFILE,
# DIAG, JOBS (default 4), NICE (default 10), CARGO_TARGET_DIR
# (default ~/.cache/twangthang-target: SHARED with Twang Thang on purpose,
# disk is tight; never point it at a fresh directory), OUT, KEYSTORE.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
ANDROID_DIR="$REPO/app/android"
LIB=libstampede.so
APP_ID=org.decented.stampede

# ── Config ──────────────────────────────────────────────────────────────────
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1)}"
BUILD_TOOLS="${BUILD_TOOLS:-$(ls -d "$SDK"/build-tools/* 2>/dev/null | sort -V | tail -1)}"
PLATFORM="${PLATFORM:-$(ls -d "$SDK"/platforms/* 2>/dev/null | sort -V | tail -1)}"
ABI="${ABI:-arm64-v8a}"
PROFILE="${PROFILE:-release}"
JOBS="${JOBS:-4}"
NICE="${NICE:-10}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/stampede-android"
OUT="${OUT:-$CACHE/out}"
RELEASE_KEYSTORE="$HOME/.stampede-release/stampede.keystore"
if [ -z "${KEYSTORE:-}" ] && [ -f "$RELEASE_KEYSTORE" ]; then KEYSTORE="$RELEASE_KEYSTORE"; fi
KEYSTORE="${KEYSTORE:-$CACHE/debug.keystore}"
DIAG="${DIAG:-0}"
MIN_SDK="$(sed -n 's/.*minSdkVersion="\([0-9]*\)".*/\1/p' "$ANDROID_DIR/AndroidManifest.xml" | head -1)"

for tool in aapt2 zipalign apksigner; do
    [ -x "$BUILD_TOOLS/$tool" ] || { echo "ERROR: $tool not in $BUILD_TOOLS" >&2; exit 1; }
done
for tool in javac; do command -v "$tool" >/dev/null || { echo "ERROR: $tool not on PATH (a JDK is needed)" >&2; exit 1; }; done
[ -x "$BUILD_TOOLS/d8" ] || { echo "ERROR: d8 not in $BUILD_TOOLS" >&2; exit 1; }
[ -f "$PLATFORM/android.jar" ] || { echo "ERROR: no android.jar in $PLATFORM" >&2; exit 1; }
[ -d "$NDK" ] || { echo "ERROR: NDK not found (set ANDROID_NDK_HOME)" >&2; exit 1; }
command -v cargo-ndk >/dev/null || { echo "ERROR: cargo-ndk missing (cargo install cargo-ndk)" >&2; exit 1; }
case "$PROFILE" in release|debug) ;; *) echo "ERROR: PROFILE must be release or debug" >&2; exit 1 ;; esac
case "$OUT" in "$REPO"|"$REPO"/*) echo "ERROR: OUT must be outside the repo (APKs are never committed)" >&2; exit 1 ;; esac

# The shared target dir and the APK cache live on a tight disk.
mkdir -p "$OUT"
FREE_KB="$(df -Pk "${CARGO_TARGET_DIR:-$HOME/.cache}" 2>/dev/null | awk 'NR==2 {print $4}')"
if [ -n "${FREE_KB:-}" ] && [ "$FREE_KB" -lt 2097152 ]; then
    echo "WARNING: only $((FREE_KB / 1024)) MB free on the build disk (< 2 GB); the build may fail. Free space first." >&2
fi

echo "ABI=$ABI PROFILE=$PROFILE JOBS=$JOBS minSdk=$MIN_SDK DIAG=$DIAG"
DEBUG_MODE=""
export STAMPEDE_DIAG="$DIAG"
# Keep one Cargo cache for quiet checks and shipping builds. Strip the copied
# shipping library at packaging time instead of rebuilding every dependency.
export CARGO_PROFILE_RELEASE_STRIP=false
if [ "$DIAG" = "1" ]; then
    DEBUG_MODE="--debug-mode"
fi
echo "NDK=$(basename "$NDK") build-tools=$(basename "$BUILD_TOOLS") platform=$(basename "$PLATFORM")"

STAGE="$OUT/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/lib/$ABI"

# ── 1. Build the cdylib ─────────────────────────────────────────────────────
# -lc++abi is REQUIRED once race-audio lands: rodio -> cpal -> oboe compiles C++, and cargo-ndk links
# libc++_static without libc++abi, so the link fails on __cxa_* symbols.
# RUSTFLAGS is set only for this command: with --target, cargo applies it to
# target artifacts only, so host (desktop) builds sharing the target dir are
# not invalidated.
echo ">>> [1/6] cargo ndk build ($PROFILE)"
export ANDROID_NDK_HOME="$NDK"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/twangthang-target}"
REL_FLAG=""
[ "$PROFILE" = "release" ] && REL_FLAG="--release"
( cd "$REPO" && RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-lc++abi" nice -n "$NICE" \
    cargo ndk -t "$ABI" -P "$MIN_SDK" -o "$STAGE/lib_cargo" \
    build $REL_FLAG -p stampede --lib -j "$JOBS" )
cp "$STAGE/lib_cargo/$ABI/$LIB" "$STAGE/lib/$ABI/"
if [ "$DIAG" = "0" ]; then
    LLVM_STRIP=""
    for candidate in "$NDK"/toolchains/llvm/prebuilt/*/bin/llvm-strip; do
        if [ -x "$candidate" ]; then LLVM_STRIP="$candidate"; break; fi
    done
    [ -n "$LLVM_STRIP" ] || { echo "ERROR: llvm-strip missing from the NDK" >&2; exit 1; }
    "$LLVM_STRIP" --strip-unneeded "$STAGE/lib/$ABI/$LIB"
fi
rm -rf "$STAGE/lib_cargo"
echo "    $LIB: $(du -h "$STAGE/lib/$ABI/$LIB" | cut -f1)"

# ── 2. Resources + manifest ─────────────────────────────────────────────────
# versionCode/versionName come from the workspace version (major*1e6 +
# minor*1e3 + patch), so they never drift from Cargo.toml.
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "ERROR: no [workspace.package] version in Cargo.toml" >&2; exit 1; }
IFS=. read -r V_MAJ V_MIN V_PAT <<< "$VERSION"
V_PAT="${V_PAT:-0}"
if [ "$V_MIN" -ge 1000 ] || [ "$V_PAT" -ge 1000 ]; then
    echo "ERROR: version $VERSION overflows the versionCode scheme" >&2; exit 1
fi
VERSION_CODE=$(( V_MAJ * 1000000 + V_MIN * 1000 + V_PAT ))

echo ">>> [2/6] aapt2 compile + link (versionCode=$VERSION_CODE versionName=$VERSION)"
"$BUILD_TOOLS/aapt2" compile --dir "$ANDROID_DIR/res" -o "$OUT/res.zip"
"$BUILD_TOOLS/aapt2" link \
    -I "$PLATFORM/android.jar" \
    --manifest "$ANDROID_DIR/AndroidManifest.xml" \
    -R "$OUT/res.zip" \
    --version-code "$VERSION_CODE" \
    --version-name "$VERSION" \
    -o "$OUT/unaligned.apk" \
    $DEBUG_MODE \
    --auto-add-overlay

# ── 3. Java -> classes.dex ─────────────────────────────────────────────────
# --release 11 is the lowest javac 21 accepts without a warning; d8 desugars
# for minSdk (lambdas included).
echo ">>> [3/6] javac + d8 (app/android/java -> classes.dex, minSdk=$MIN_SDK)"
mkdir -p "$STAGE/classes" "$STAGE/dex" "$STAGE/test-classes"
find "$ANDROID_DIR/java" -name '*.java' | sort > "$STAGE/sources.txt"
javac --release 11 -Xlint:all -d "$STAGE/classes" -cp "$PLATFORM/android.jar" @"$STAGE/sources.txt"

# ApkCheck (the package/certificate comparison that gates every install) is
# unit-tested on the host JDK against the vectors the Rust mirror uses too; the
# test is not in the APK.
command -v java >/dev/null || { echo "ERROR: java not on PATH (a JDK is needed)" >&2; exit 1; }
javac --release 11 -Xlint:all -d "$STAGE/test-classes" \
    "$ANDROID_DIR/java/org/decented/stampede/ApkCheck.java" \
    "$ANDROID_DIR/test/org/decented/stampede/ApkCheckTest.java"
java -cp "$STAGE/test-classes" org.decented.stampede.ApkCheckTest "$ANDROID_DIR/test/apk-check-vectors.tsv"

D8_MODE=--release
[ "$DIAG" = "1" ] && D8_MODE=--debug
mapfile -t CLASS_FILES < <(find "$STAGE/classes" -name '*.class' | sort)
"$BUILD_TOOLS/d8" "$D8_MODE" --min-api "$MIN_SDK" --lib "$PLATFORM/android.jar" \
    --output "$STAGE/dex" "${CLASS_FILES[@]}"
[ -f "$STAGE/dex/classes.dex" ] || { echo "ERROR: d8 produced no classes.dex" >&2; exit 1; }

# ── 4. Native library + dex ─────────────────────────────────────────────────
echo ">>> [4/6] adding lib/$ABI/$LIB and classes.dex"
( cd "$STAGE" && zip -q -r "$OUT/unaligned.apk" lib )
zip -q -j "$OUT/unaligned.apk" "$STAGE/dex/classes.dex"

# ── 5. Align (16 KB pages for Android 15+ devices) ──────────────────────────
echo ">>> [5/6] zipalign"
"$BUILD_TOOLS/zipalign" -f -P 16 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"

# ── 6. Sign (the Stampede key, or a throwaway fallback) ──────────────────────
if [ ! -f "$KEYSTORE" ]; then
    echo ">>> generating throwaway debug keystore at $KEYSTORE"
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android \
        -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
fi
APK="$OUT/stampede_${VERSION}_${ABI}.apk"
echo ">>> [6/6] apksigner ($KEYSTORE)"
"$BUILD_TOOLS/apksigner" sign \
    --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
    --ks-key-alias androiddebugkey \
    --out "$APK" "$OUT/aligned.apk"

rm -f "$OUT/unaligned.apk" "$OUT/aligned.apk" "$OUT/res.zip" "$APK.idsig"
rm -rf "$STAGE"
ln -sf "$(basename "$APK")" "$OUT/stampede.apk"

echo
echo "BUILT: $APK ($(du -h "$APK" | cut -f1))"
"$BUILD_TOOLS/aapt2" dump badging "$APK" | grep -E "^(package|minSdkVersion|targetSdkVersion|application-label|native-code|uses-permission)" || true
echo
echo "  adb install -r $OUT/stampede.apk"
echo "  adb shell am start -n $APP_ID/.StampedeActivity"
echo "  adb logcat -s stampede:V"
