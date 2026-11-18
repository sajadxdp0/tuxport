<p align="center">
  <img src="website/dist/favicon.svg" width="72" alt="Tuxport logo">
</p>

<h1 align="center">Tuxport</h1>

<p align="center">
  A Linux terminal for Android.<br>
  Alpine Linux is bundled with the app. No root access required.
</p>

<p align="center">
  <a href="https://github.com/sajadxdp0/tuxport/releases"><strong>Download</strong></a> ·
  <a href="https://sajadxdp0.github.io/tuxport/">Website</a> ·
  <a href="https://sajadxdp0.github.io/tuxport/privacy.html">Privacy policy</a>
</p>

<p align="center">
  <img src="docs/screenshots/terminal-demo.gif" width="360" alt="Tuxport booting Alpine Linux, running uname, ls and top on an Android phone">
</p>

## What Tuxport does

Tuxport runs a complete Linux command line on an unrooted Android phone or
tablet. Alpine Linux 3.24 ships inside the APK, so the first launch opens a
root shell without setup, downloads or a rooted device. A Debian system is
available from Settings.

Inside the terminal, the environment is a normal Linux system:

- `apk` installs packages from the Alpine repositories, over the phone's
  network.
- Unmodified ARM64 binaries run as they are: Python, Node.js, Go, Rust,
  tmux, Vim, Codex CLI.
- Programs keep running while the screen is off, and notifications report
  finished commands, bells and listening ports.
- The terminal is Ghostty's libghostty-vt, with tabs, an extra keys row,
  scrollback find and safe paste confirmation.
- A privacy lock requires biometrics to reopen the app, and a diagnostic log
  exporter helps with problems.

The execution mode is selectable. The default translates guest instructions
into native code for speed. An interpreter mode runs every instruction step
by step and exists for troubleshooting.

## How it works

Tuxport executes unmodified ARM64 Linux binaries inside the Android app
process. A CPU engine translates ARM64 instructions into ARM64 code and
interprets the instructions it does not translate. A user-space kernel
implements the Linux system calls, and the terminal runs on Ghostty's
libghostty-vt.

## Requirements

Android 11 or later on a 64-bit ARM device.

## Development

Requirements: JDK 17 or later, the Android SDK with platform 37 and NDK
29.0.14206865, Rust through rustup (`rust-toolchain.toml` pins the version),
`cargo install cargo-ndk --locked`, and Zig 0.17.x on `PATH` or as `zig.path`
in `android/local.properties`.

From the repository root:

```sh
scripts/fetch-rootfs      # Alpine minirootfs and the bundled packages
scripts/fetch-ghostty     # libghostty-vt source and its Zig packages

cd android
./gradlew installDebug                # build and install on a device
./gradlew assembleRelease             # release APK with R8 shrinking
./gradlew connectedDebugAndroidTest   # instrumentation tests on a device
```

The Rust workspace tests need an ARM64 Linux host or container.
`scripts/dev cargo test --workspace` runs them in the project container.

## Documentation

- [Architecture overview](docs/design/README.md) and the
  [module index](.agents/MODULES.md) for navigating the source.
- [Module maps](.agents/maps) for current behavior and tests.
- Coding agents should start with [AGENTS.md](AGENTS.md).

## License

Tuxport is Copyright (C) 2026 Sajad, licensed under the
[GNU Affero General Public License, version 3 or later](LICENSE).

Bundled third-party software keeps its own licenses: libghostty-vt and its
dependencies, JetBrains Mono, and the Alpine Linux packages. Read them in
Settings, under Open source licenses.
