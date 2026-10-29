# Stampede

Laser-eyed ostrich racing: a Mario Kart-style arcade racer, native Rust, no
game engine. Android (landscape, arm64) and desktop Linux from one codebase.
App shell API: [docs/api/app-shell.md](docs/api/app-shell.md).

## Source dependencies

The workspace uses sibling source checkouts as path dependencies. Before
building, provide `../forgesworn/fray` (the `fray-net` and `fray-relays`
crates) and `../TwangThang` (the `decented-signet` crate and its path
dependencies). These sources are not bundled with this repository.
`Cargo.lock` pins the external registry dependencies.

## Desktop

```sh
export CARGO_TARGET_DIR=~/.cache/twangthang-target   # shared with Twang Thang (disk is tight)
cargo run -p stampede                                 # 1600x720 window, starts on the splash
cargo run -p stampede -- --screen home                # skip the splash
STAMPEDE_NO_VSYNC=1 cargo run -p stampede             # if frames stall ~1 s each (X11 Fifo)
cargo test -p stampede
```

Keys: Esc = Back. Mouse: left button = a finger, right button = a second finger.

Headless screenshot of any registered screen (no window; needs Vulkan or GL):

```sh
cargo run -p stampede -- --shot home --out docs/local/shots/home.png --size 2400x1080 --ppp 2.625
```

## Linux packages (.deb and AppImage)

Both scripts do a release build of the current tree (shared
`CARGO_TARGET_DIR=~/.cache/twangthang-target`; cargo waits if another build
holds the lock), take the version from the workspace `Cargo.toml`, and write
outside the repo to `~/.cache/stampede-linux/out/` (`OUT` overrides). Re-run
them for each release after bumping the version.

```sh
tools/linux/build-deb.sh        # stampede_<version>_amd64.deb
tools/linux/build-appimage.sh   # Stampede-<version>-x86_64.AppImage
SKIP_BUILD=1 tools/linux/build-deb.sh   # reuse the release binary already built
```

Needs `dpkg-deb`, `dpkg-shlibdeps` (dpkg-dev), Python 3 with Pillow + numpy (icons
are rendered from `tools/icon/`) and, for the AppImage, `curl`. `appimagetool` is
taken from `PATH`, else a pinned, checksum-verified release is downloaded to
`~/.cache/stampede-linux/tools/`. `lintian`, `desktop-file-validate` and
`appstreamcli` are run if installed. Icons are cached in
`~/.cache/stampede-linux/` and re-rendered when `tools/icon/` changes. Desktop
file, AppStream metainfo and the icon sizes live in `tools/linux/`.

```sh
sudo apt install ./stampede_0.1.0_amd64.deb      # installs /usr/bin/stampede
chmod +x Stampede-0.1.0-x86_64.AppImage && ./Stampede-0.1.0-x86_64.AppImage
```

Caveats:

- **GPU and system libraries come from the host.** Neither package bundles
  Vulkan/Mesa/EGL, X11, Wayland or ALSA. The deb Depends on `libvulkan1 | libegl1`,
  the X11/xkbcommon/Wayland client libs and `libasound2t64 | libasound2`, and
  Recommends `mesa-vulkan-drivers` and `xclip | wl-clipboard` (room-code paste).
- **glibc floor.** The binary is built on the dev machine (Linux Mint 22, glibc
  2.39), so the deb needs `libc6 >= 2.39` and the AppImage runs on distros at
  least that new. Build on an older distro (container) for wider reach.
- **Stalled frames (Intel Xe on X11).** With Fifo/vsync the surface acquire can
  block ~1 s per frame. Run with `STAMPEDE_NO_VSYNC=1`; the menu entry has a
  "vsync off" action that does this. It is not the default (it would tear and
  spin the CPU on healthy drivers).
- **AppImage needs FUSE 2** (`libfuse2` / `libfuse2t64`) on the host; without
  it run `./Stampede-*.AppImage --appimage-extract-and-run` (the extract target
  must not be a `noexec` mount).
- The window sets its Wayland `app_id` / X11 class to `org.decented.stampede`,
  matching the `.desktop` file so docks show the right icon.
- Signing is not done for either package; the deb's `Maintainer` is a
  placeholder (override with `MAINTAINER="Name <mail>"`).
- Data lives in `$XDG_DATA_HOME/stampede` (`~/.local/share/stampede`), not
  touched by install or uninstall.

## Android APK (no Gradle)

Needs the Android SDK + NDK (`~/Android/Sdk`), a JDK, `cargo-ndk` and the
`aarch64-linux-android` rust target.

```sh
tools/build-apk.sh                 # release, arm64-v8a
PROFILE=debug tools/build-apk.sh   # unoptimised (slow on device)
ABI=x86_64 tools/build-apk.sh      # emulator
DIAG=1 tools/build-apk.sh          # unstripped .so + debuggable (never ship)
```

Output: `~/.cache/stampede-android/out/stampede_<version>_<abi>.apk` and a
`stampede.apk` symlink. `JOBS` (4), `NICE` (10), `CARGO_TARGET_DIR` (default
`~/.cache/twangthang-target`, shared on purpose) and `OUT` are overridable; the
script warns under 2 GB free.

Signing: `~/.stampede-release/stampede.keystore` if present, else a throwaway
key generated at `~/.cache/stampede-android/debug.keystore`. An APK signed with
the throwaway key cannot be updated in place by a build signed with the real
key: `adb uninstall org.decented.stampede` first.

```sh
adb install -r ~/.cache/stampede-android/out/stampede.apk
adb shell am start -n org.decented.stampede/.StampedeActivity
adb logcat -s stampede:V                      # frame times every 5 s
adb exec-out screencap -p > shot.png
adb shell am start -a android.intent.action.VIEW -d "stampede://join#KWTQPM" org.decented.stampede
```

### Notes on the Android port

(Ported from Twang Thang's platform layer.)

- **Package** `org.decented.stampede`, lib `libstampede.so`, activity
  `StampedeActivity` (NativeActivity subclass; only `onNewIntent` -> `setIntent`).
  `singleTask`, `sensorLandscape`, minSdk 24, targetSdk 35. Installs next to Twang Thang.
- **Join links** `stampede://join#CODE`: read from `getIntent()` over JNI at
  start, on resume and every 250 ms, then consumed (`setData(null)`).
- **System Back** is `Screen::back`; Back on the first screen sends the game to the background.
- **Immersive fullscreen**, drawing under the camera cutout (left/right edge in
  landscape); the game reads all four cutout insets over JNI and pads by them.
- **Backgrounded**: the surface is dropped and rebuilt, `Ctx::muted` is set, the
  event loop sleeps, the profile is saved. The wake lock is held only while a screen's
  `keep_awake()` is true.
- Permissions: INTERNET, ACCESS_NETWORK_STATE, ACCESS_WIFI_STATE,
  CHANGE_WIFI_MULTICAST_STATE (Nearby play), VIBRATE, WAKE_LOCK.
