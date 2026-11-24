# android/build Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/build.md` for the paired decision history (not yet created; pre-split decisions D2–D7, D10, D12–D14, D20–D22, D24–D26 and D28 stay in `.agents/decisions/android/native.md`).
>
> Verified: 2026-11-15 (split out of the android/native full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-19 (targeted verification only since the full mapping; latest change: the Zig build passes the lib's LIB_VERSION as -Dversion-string; release signing and version facts refreshed)

## Responsibilities

`android/build` builds and packages the APK, native libraries, guest browser executable,
bundled assets, notices and released source archives. Root: `hosts/android/build.rs` and
`Cargo.toml`, Android Gradle files and wrapper, `android/app/src/main/AndroidManifest.xml`,
`android/app/src/debug/`, `android/licenses/`, `android/app/src/main/assets/notices/`,
`scripts/fetch-ghostty` and `scripts/release-sources`.

Runtime JNI, `MainApplication` and `open-url.c` remain `android/native`; instrumentation source
is `android/tests`; notice UI is `android/licenses`, font assets `android/vtrender` and rootfs
fetch/provisioning `android/setup`. CI orchestration remains `harness`.

## Key types

- Gradle tasks: `CargoNdkTask` (one per variant, `buildRust<Variant>`, owning the workspace,
  process execution and cargo lookup), `RootfsAssetTask` (one `rootfsAsset` shared by all variants);
  helpers `byVersion` (compares file names by their runs of digits, then by how many runs),
  `newestFetched` (the newest file in `target/rootfs` whose name matches), `fetchedPackages`
  (the newest APK for each requested name, null if any is missing), used by `bashPackages`
  (the newest of bash, readline, libncursesw and ncurses-terminfo-base, null when any is missing)
  and `procpsPackages` (procps-ng, libproc2, libintl, utmps-libs, skalibs-libs and the two ncurses
  packages, null when any is missing), `bubblewrapPackages` (bubblewrap and libcap2, null when either
  is missing), and `alpineBranch` (from the tarball's name, `edge` without one).
- `BrowserAssetTask` builds the static guest browser client as a generated asset.

## Public entry points

- Build: `./gradlew assembleDebug lintDebug` (what CI runs), `assembleRelease` (the optimized APK), `connectedDebugAndroidTest` (with
  `-Pandroid.testInstrumentationRunnerArguments.network=true` for the download of a distribution),
  `scripts/fetch-ghostty [dir]` (default `target/ghostty`), `scripts/release-sources [dir]` (default
  `target/release-sources`).

## Data flow / lifecycle

### Build

- `CargoNdkTask` runs `cargo ndk -t arm64-v8a --platform 30 -o <build/generated/rust/<variant>/jniLibs>
  build -p tuxport-android --profile <profile>`, with `CARGO_TARGET_DIR=target/android` and both
  `ANDROID_NDK_HOME` and `ANDROID_NDK_ROOT` set to AGP's NDK, using `~/.cargo/bin/cargo` when it is
  executable; it is never up to date. Debug variants add the `test-hooks` and `borrow-check`
  features and use Cargo's `release` profile. The release APK selects the workspace's
  `android-release` profile, which inherits `release` and enables cross-crate LTO.
- Gradle resolves Zig from a nonblank, trimmed `zig.path` in `android/local.properties`, then the
  `ZIG` environment variable, then `zig` on `PATH`. `BrowserAssetTask` invokes that executable;
  `CargoNdkTask` passes the same value as `ZIG` to cargo and its build script. The local file is
  ignored by Git, and its contents are read through a Gradle file provider for configuration-cache
  tracking; both tasks declare the selected executable as an input.
- On an Android target `build.rs` asserts the libghostty-vt source (`build.zig` in
  `target/ghostty/libghostty-vt-1.0.0`, or `TUXPORT_GHOSTTY_SRC`) and the release's Zig packages
  beside it (the same path with `-deps`), reads the release's `LIB_VERSION`, panics on any
  architecture but aarch64, and runs `$ZIG`
  (or `zig` on `PATH`) `build -Demit-lib-vt -Dcpu=baseline -Doptimize=ReleaseFast
  -Dtarget=aarch64-linux-android -Dversion-string=<LIB_VERSION> --system <deps>` into `OUT_DIR`,
  so Zig takes the packages from that directory and never fetches. The version flag is passed
  because libghostty-vt otherwise derives its version from git, and the unpacked source sits
  inside this repository's work tree, so that reads this repository's state: on a tagged
  checkout the lib's check that the checked-out tag matches its version panics on the
  repository's own tag (v1.1.0's CI failed this way; reproduced locally at the tag). Its global cache is the source's `.zig-global-cache`, beside
  Zig's local `.zig-cache` there, so builds of every profile (Gradle's, clippy's) share both. It
  then compiles `src/vt.c` (C11, extra warnings) with `cc`, against the installed headers and
  Wuffs's single-file header among the packages (`<deps>/<hash>/release/c`, the hash read from the
  source's `pkg/wuffs/build.zig.zon`; vt.c decodes PNG images with the Wuffs in the archive), and
  links `libghostty-vt.a` statically into `libtuxport.so` (with uucode, simdutf, Highway, Wuffs, the
  translate-c helpers and Zig's compiler_rt); `vt.rs` also links the system's libjnigraphics. The
  build finds the NDK through `ANDROID_NDK_HOME`, and libghostty-vt 1.0.0 requires Zig 0.17.x,
  which its own build checks (`minimum_zig_version`). Every Android build, CI's included,
  therefore needs Zig and the fetched release.
- `scripts/fetch-ghostty [dir]` handles the release's source tarball and its dependency tarball
  (`release.files.ghostty.org/libghostty-vt/<version>/`, each with its own pinned sha256): it keeps
  a tarball whose sha256 matches, else downloads it (`curl --retry 3` to a `.part` file, checked,
  renamed), and unpacks each into `<name>.part` and moves its directory into place whole, only
  when that directory is missing. It runs no Zig and needs no proxy handling.
- Toolchain: AGP 9.4.1 with its built-in Kotlin, Gradle 9.9.1, Java 17, NDK 29.0.14206865; plugins
  and dependencies from `google()` and `mavenCentral()` (project repositories fail the build). The
  version catalog (`android/gradle/libs.versions.toml`) holds the libraries' versions, and AGP's
  is in `android/build.gradle.kts`. Android libraries: `androidx.core` 1.19.1,
  `androidx.biometric:biometric` 1.1.0 (including its merged biometric permission), `androidx.preference:preference` 1.2.1, `androidx.browser:browser` 1.10.0,
  `androidx.webkit:webkit` 1.18.0, `androidx.recyclerview:recyclerview` 1.2.1 and Material Components 1.14.0 (which
  brings AppCompat and androidx.activity); tests use androidx.test's runner 1.7.0 and ext.junit
  1.3.0.
- Assets: the newest Alpine minirootfs tarball in `target/rootfs` by version (as `rootfs.tgz`,
  since AAPT would unpack `.gz`), the newest `doas.apk`, and bash's four packages under `bash/`
  with their own file names, plus procps-ng's seven packages under `procps/` (including copies of
  ncurses so either bundle installs independently), and bubblewrap/libcap2 under `bubblewrap/`
  (all by `byVersion`; `rootfsAsset` fails with "run
  scripts/fetch-rootfs first" when any is missing), checked-in `notices/`, and
  the terminal font (`fonts/`, `android/vtrender`); `BuildConfig.ALPINE_BRANCH` carries the
  branch.
- SDK: min 30, compile and target 37; no ABI filter (only `arm64-v8a` is built); uncompressed
  native libraries (`useLegacyPackaging = false`) so 16 KB-aligned `LOAD` segments (NDK 29's
  default; the build sets no page-size flag) map directly from the APK.
- Warnings fail: every Kotlin compilation (the app and the instrumentation tests, local builds
  included) treats compiler warnings as errors, and lint treats its warnings as errors, except
  `GradleDependency`, `AndroidGradlePluginVersion`, `OldTargetApi`, `ExpiringTargetSdkVersion` and
  `OutdatedLibrary`, which follow data outside the repository and are informational.
- Release: signed with the release keystore when `android/local.properties` (gitignored)
  carries the `signing.*` entries, else the debug key, so versionCode 10 and versionName 0.0.3.
  R8 uses the optimized Android defaults and shrinks unused resources. `app/proguard-rules.pro`
  keeps app JNI declarations and the public void `VtSession.on*` callbacks looked up by the native
  pump; callbacks outside that pattern need a matching keep rule. The release mapping is under
  `app/build/outputs/mapping/release/`.

### Notices

- `app/src/main/assets/notices/` holds static HTML, a shared stylesheet and `index.json`, packaged
  without a Gradle generation task. The catalog lists, in order: Tuxport (its notice and the
  repository's `LICENSE`); Fuchsia's
  BSD-style licence for the line-discipline parts adapted from Starnix (`kernel/tty`); the
  terminal's components with their texts from `android/licenses/` (libghostty-vt 1.0.0, uucode
  with the Unicode and Hoehrmann licences, simdutf, Highway and Wuffs under their MIT or BSD option,
  translate-c, Zig's compiler_rt, JetBrains Mono's OFL); Alpine (each package's version, licence, origin and commit
  from the tarball's `lib/apk/db/installed`, then the bundled packages' from their `.PKGINFO`, doas
  and bash's four and procps-ng's five additional packages in sections of their own (shared ncurses
  counted only once), plus bubblewrap and libcap2, all counted in the entry's package count (28) and
  licences); the Android libraries on the
  runtime classpath; the Rust crates; SQLite (its version from libsqlite3-sys's `sqlite3.h`); full
  texts of GPL-3.0, GPL-2.0, LGPL-2.1, Apache-2.0 and MPL-2.0. `LicenseCatalog` (android/licenses)
  reads stable IDs, names, versions, chosen licenses and HTML file names, plus optional package
  counts and standalone-license kinds. Detail pages retain upstream alternatives and notices.
- Dependency and bundled-package upgrades must update these files in the same change; Gradle
  no longer discovers licenses or rejects unknown Android groups. Compare both runtime
  classpaths, the Android-target normal Rust dependency graph (excluding workspace members and
  proc-macros), and the selected Alpine artifacts. The initial debug/release inventories agree.
- `LicenseTest` checks the catalog's unique IDs, readable files and component notice coverage;
  it is not a general dependency freshness check. `android/README.md` describes maintenance.
- The component texts in `android/licenses/` (16 files) remain upstream reference copies,
  maintained with their corresponding HTML notices; the release's
  `deps/libghostty-vt-third-party.json` lists which projects are linked (translate-c among them,
  though nothing of it shows as an object of its own in `libghostty-vt.a`).

### Manifest

- The application declares `android:localeConfig="@xml/locale_config"` for Android 13+'s
  system per-app language picker. The explicit list is English (`en`), Simplified Chinese
  (`zh-CN`), Traditional Chinese (`zh-Hant`), Korean (`ko`) and Japanese (`ja`), matching
  the app's resources rather than dependency translations. Android 11–12
  ignore the attribute and keep following the system language; the application suppresses
  lint's `UnusedAttribute` for this intentional version-specific behavior.
- Manifest application `MainApplication` lives in its own file and installs android/privacy's shared page gate with `Privacy.install` in `onCreate`; non-exported `NotificationActivity` (android/privacy) authenticates private report-copy actions, while non-exported `Notifications.ReportReceiver` copies ordinary reports without opening a page. AndroidX Biometric is listed in the static Android library notice; both runtime dependency inventories were checked.
- `MainApplication` also installs `Diagnostics` after the privacy gate. Non-exported
  `DiagnosticProvider` uses the existing AndroidX FileProvider with manifest path metadata,
  granting only generated archives from `cache/diagnostic-exports`; no new permission or
  dependency is required. Retained diagnostic files use the app's no-backup directory.
- Launcher, settings, licenses list and license reader activities (the application theme
  `Theme.Tuxport`, Material 3; the
  launcher overrides it with `Theme.Tuxport.Terminal`; settings and licenses leave `uiMode` out of
  their configuration changes; the non-exported reader handles no configuration changes itself),
  the `specialUse` foreground service with its subtype property, the
  documents provider, backup turned off (`allowBackup`, `fullBackupContent`, data extraction
  rules), normal permissions plus `POST_NOTIFICATIONS`, which `MainActivity` and Settings can request at run time,
  and `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` for the terminal banner’s user-initiated system dialog.
  The app requests no storage permission. The debug source set's network security config allows cleartext to
  127.0.0.1 only (for the download tests).

### Released sources

- `scripts/release-sources` refuses to run with tracked changes in the work tree and writes, under
  the short `HEAD` revision: `tuxport-<rev>.tar.gz` (`git archive HEAD`), `tuxport-<rev>-vendor.tar.gz`
  (`cargo vendor --locked` of the whole workspace), `libghostty-vt-<version>.tar.gz` and
  `libghostty-vt-<version>-deps.tar.gz` (the release and its Zig packages, copied as
  `fetch-ghostty` checked them; together about 10 MB), and
  `alpine-<version>-gpl-sources.tar` (uncompressed, with a README listing the packages).
- The Alpine part takes the tarball `scripts/fetch-rootfs` prints last and the bundled packages
  it prints before it, picks packages whose licence (the tarball's `lib/apk/db/installed`, each
  package's `.PKGINFO`) contains `GPL` (LGPL included; so bash, readline, procps-ng/libproc2 and
  libintl, bubblewrap and libcap2, not doas, ncurses, utmps-libs or skalibs-libs),
  downloads each origin's
  aports directory at its commit (GitLab archive of `main/<origin>`), and for every file in the
  APKBUILD's `sha512sums` hashes the copy in that directory or fetches it from Alpine's distfiles
  mirror (the branch's directory, then `edge`); any miss or mismatch stops the script. APKBUILDs
  are never executed.

### Guest browser asset

- `BrowserAssetTask` builds `hosts/android/open-url.c` with Zig as static aarch64-linux-musl,
  packaging `tuxport-open-url` as a generated asset for either distro; no guest package is needed.
  The bundled musl copyright is in `android/licenses/` and the offline notices.

## Dependencies (inbound / outbound)

### Inbound

- CI (`harness`) runs `assembleDebug lintDebug` and cargo-ndk clippy.
- `android/native` and `android/vtcore` require the native build; `android/tests` uses the debug
  app and instrumentation runner. `android/setup` reads assets and `BuildConfig`;
  `android/licenses` reads static notice assets.

### Outbound

- cargo-ndk, NDK 29, AGP, Gradle and the Android libraries listed above.
- Zig 0.17.x, libghostty-vt 1.0.0 source/dependency archives and `cc`.
- `android/native` runtime sources, terminal sources and `android/setup`'s `scripts/fetch-rootfs` output.
- Release archives use git, `cargo vendor`, `fetch-ghostty`, `fetch-rootfs`,
  gitlab.alpinelinux.org, distfiles.alpinelinux.org, curl, tar, awk and a SHA-512 tool.

## Invariants and gotchas

- The global cache a `--system` build uses must never have held a package's archive: the user's
  `~/.cache/zig` usually has, and Zig 0.17 then panics ("reached unreachable code"), so `build.rs`
  gives the source a cache of its own, which nothing fetches into.
- `build.rs` reruns only when `vt.c`, the source's `LIB_VERSION` or `build.zig.zon`,
  `TUXPORT_GHOSTTY_SRC` or `ZIG` change, not for other libghostty-vt files or `ANDROID_NDK_HOME`.
- The Zig build must keep passing the lib's version (`-Dversion-string`) or otherwise avoid
  git: the source is unpacked inside this repository, so libghostty-vt's git detection reads
  the repository's tags, and a release-tag checkout would panic its version check
  (`v1.1.0` does not match the lib's `v1.0.0`).
- vt.c's Wuffs header is found through the release's layout: the dependency named `wuffs` in
  `pkg/wuffs/build.zig.zon`, the dependency tarball's `<hash>` directories and the file
  `wuffs-v0.4.c`. An upgrade that changes any of them fails the build ("no Wuffs …" or no hash)
  until `build.rs` follows; the package is there whenever libghostty-vt builds, since it links Wuffs
  for Kitty graphics.
- The K2 compiler reports unused variables and never-read assignments only with its extra
  warnings, which are off: they flag a variable read only by a local function as never read.
- `release-sources` relies on `fetch-ghostty` printing `fetch-ghostty: <source>` with the two
  tarballs at `<source>.tar.gz` and `<source>-deps.tar.gz`, and on `fetch-rootfs` printing the
  bundled packages' paths (ending in `.apk`) and the tarball's path last.

## Confirmed bugs / technical debt

- The Alpine tarball's checksum is checked only in `scripts/fetch-rootfs`.
- `scripts/fetch-ghostty` checks only the pinned sha256s; the release's minisign signatures are to
  be checked by hand once when upgrading, before pinning the new sha256s.
- `scripts/release-sources` takes the Alpine sources from `fetch-rootfs`'s pinned tarball and
  packages while the APK bundles the newest ones in `target/rootfs`, so the two can differ; its
  aports path is fixed to `main/`; and it vendors the whole workspace although its header speaks
  of the crates linked into the app.

## Open questions

- Is the release variant meant to ship as configured?
