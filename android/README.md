# Tuxport Android app

The Gradle project for the Android shell (design doc §3 and the `android/*` maps; terminals in the `kernel/tty` map):

- `app/`: the Kotlin app. `TerminalService` is a `specialUse` foreground service that
  owns the guest kernel and the terminal sessions; `MainActivity` shows them with tabs
  and a row of extra keys. The terminal emulator is libghostty-vt from Ghostty, linked into
  `libtuxport.so` (`hosts/android/src/vt.c`, `vt.rs`), and drawn by `VtView` with the bundled
  JetBrains Mono font. `Rootfs` imports Alpine from the APK assets on first launch,
  picks the fastest mirror, and can replace it with Debian (downloaded and checked
  against a pinned sha256 digest). `SettingsActivity`, opened from the tab bar's menu,
  switches the distribution, resets the rootfs, chooses the package mirror, the login
  account and the execution mode (Tier 1 by default; Tier 0 for troubleshooting in both debug
  and release builds).
  Each tab runs `assets/login.sh` as root: it picks the
  login account (the first regular user by default), gives a regular user doas or sudo the
  first time it logs in (the doas package from `scripts/fetch-rootfs` is in the assets),
  installs the bundled bash, procps-ng and bubblewrap packages offline on a new Alpine system,
  and switches to it with `su` (`--session-command` where su is util-linux's, so that the
  shell keeps its controlling terminal and job control). `Native` is the JNI interface to
  `libtuxport.so`, which is built from `hosts/android` with cargo-ndk.

## Building

Requirements: the Android SDK with NDK 29.0.14206865, JDK 17 or later,
`rustup target add aarch64-linux-android`, `cargo install cargo-ndk`, Zig 0.17.0 on `PATH`
(or in `ZIG`), the rootfs tarball (`scripts/fetch-rootfs`) and the libghostty-vt source release
with its Zig packages (`scripts/fetch-ghostty`), both run from the repository root. The Zig
build then runs offline. Zig also builds the static Linux browser launcher from
`hosts/android/open-url.c`; its musl libc notice is included in the APK.

Gradle resolves Zig from `zig.path` in `android/local.properties` first, then the `ZIG`
environment variable, then `zig` on `PATH`. A missing or blank local value falls back to
the environment. Both the browser launcher and Rust native build use this setting.
For Android Studio on a Mac with Homebrew, add this to the ignored `local.properties`:

```properties
zig.path=/opt/homebrew/bin/zig
```

```sh
./gradlew installDebug                 # build and install on a connected arm64 device
./gradlew connectedDebugAndroidTest    # instrumentation tests on a device or emulator
./gradlew assembleRelease              # build app/build/outputs/apk/release/app-release.apk
```

Both APKs build `libtuxport.so` for `arm64-v8a`. Debug uses Cargo's `release` profile to keep
the interpreter fast. Release uses `android-release` for cross-crate LTO and a smaller library.

The release APK uses R8 and resource shrinking and is signed with the local debug key.
`app/proguard-rules.pro` preserves JNI methods and the `VtSession.on*` callbacks Rust looks up
by name. Archive `app/build/outputs/mapping/release/mapping.txt` with its APK to decode stack traces.

Run the instrumentation tests on both a 4K-page
and a 16K-page system image. `downloadedDistributionsRun` downloads Debian
(about 50 MB) and the browser bridge test installs tmux/screen in a separate temporary store. Both require
`-Pandroid.testInstrumentationRunnerArguments.network=true`. An emulator started
with `-http-proxy` sends connections to 127.0.0.1 to the proxy too, which breaks
`downloadsResumeWhereTheyBrokeOff`. Setting the system proxy instead
(`adb shell settings put global http_proxy 10.0.2.2:<port>`) only affects the app's own
HTTP downloads and leaves 127.0.0.1 alone; clear it afterwards with
`adb shell settings put global http_proxy :0`. Deleting the setting instead leaves
`global_http_proxy_host` and `global_http_proxy_port` behind, and the proxy stays in effect
(with an empty exclusion list, so 127.0.0.1 goes to the proxy too) until the next reboot.

## Open source licenses

The notices under Settings → Open source licenses are checked-in resources in
`app/src/main/assets/notices/`; Gradle does not generate them. `index.json` records each
entry's stable ID, project name, version, short license label and HTML file. The optional
`packageCount` produces the localized Alpine summary, and `kind: "license"` identifies a
standalone license text, hidden from the main list but reachable through component detail
links. Each HTML document links to the shared `style.css`. The reader
supplies `/theme/colors.css` from the current Material theme, opens external web links in
Custom Tabs and needs no network connection to read a notice.

Keep notices in the same change as dependency updates:

- Tuxport's notice includes the repository's `LICENSE`, copyright, warranty and source link.
- Fuchsia, libghostty-vt and its linked components, and JetBrains Mono retain their original
  notices; `licenses/` keeps the upstream source copies for comparison.
- Rust entries cover the normal dependencies reachable from `tuxport-android` on
  `aarch64-linux-android`, excluding workspace members, build dependencies and proc-macros.
  Record the chosen MIT license when offered as an alternative, and retain the upstream
  expression and any additional notices in the detail page. Keep separate entries for
  simultaneously linked versions, such as jni-sys 0.3 and 0.4.
- `android.html` lists the resolved Android runtime libraries, including transitive ones;
  check both debug and release runtime classpaths when changing the version catalog.
- `alpine.html` records the bundled rootfs and doas/bash/procps/bubblewrap packages, with versions,
  license expressions and aports commits. Compare with the artifacts Gradle selects in
  `target/rootfs`, and count shared ncurses packages only once.
- Preserve license wording, copyright notices and all required component notices when
  editing HTML. Use paragraphs and headings for reflow; do not shrink fixed-width text.
  Keep IDs stable across upgrades, and update the separate source bundles as before
  (`scripts/release-sources`).

`LicenseTest` checks coverage and offline rendering/scroll restoration without starting the
kernel or changing its store. It does not detect every dependency upgrade; reviewing the
resolved dependencies and bundled package metadata remains part of updating the static files.

```sh
./gradlew assembleDebug lintDebug
./gradlew connectedDebugAndroidTest \
  -Pandroid.testInstrumentationRunnerArguments.class=io.github.sajadxdp0.tuxport.LicenseTest
```
