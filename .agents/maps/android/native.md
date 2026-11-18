# android/native Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/native.md` for the paired decision history, including pre-split decisions for android/build and android/tests.
>
> Verified: 2026-11-10 (full mapping: 3 research concerns; 16 claims checked by 2 independent verifiers — 14 confirmed and 2 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: split android/build and android/tests into separate maps)

## Responsibilities

`android/native` connects the Kotlin app to the Rust kernel: the kernel handle table, JNI
entry points, error and panic handling, stderr to logcat, and the guest browser client.

Root: `hosts/android/src/lib.rs`, `hosts/android/open-url.c`,
`android/app/src/main/java/io/github/sajadxdp0/tuxport/Native.kt` and `MainApplication.kt`.

The build, manifests, assets and release packaging are `android/build`; instrumentation is
`android/tests`. `Machine` lifecycle belongs to `kernel/boot`, process API to `kernel/process`
and store calls to `kernel/store`. Terminal shim/JNI ownership remains `android/vtcore`,
with view and rendering portions in `android/vtview` and `android/vtrender`.

## Key types

- `MACHINES` — a mutex-protected map from handle (`jlong`, from `NEXT_HANDLE`, counting up from 1
  and never reused in a process) to `Arc<Machine>`; a poisoned lock is recovered. `vt.rs` looks
  kernels up in the same table.
- `Native` — `internal object` with 37 `@JvmStatic external` functions, 17 of them
  `@Throws(IOException::class)`; its initialiser loads the library and calls the private
  `init(BuildConfig.DEBUG)`.
- `guarded!` — the macro every JNI body runs in: `(name, fallback, body)`, or
  `(name, throws env, fallback, body)` for the functions Kotlin declares `@Throws`.

## Public entry points

- Init: `init(verbose)` (private; stderr redirect, debug log).
- Store: `importRootfs`, `syncRootfs`, `writeFile`, `readFile` (null on `ENOENT`).
- Browse: `browseStat`, `browseList`, `browseOpen`, `browseSniff`, `browseSearch` (read);
  `browseCreate`, `browseRemove`, `browseRename`, `browseMove`, `browseOpenWrite`, `browseWritten`
  (through the store's change path; `browseWritten` is not `@Throws` and ignores every error).
- Kernel: `start(store, tier)` (1 = Tier 1, else Tier 0; the kernel starts with
  `debug.exception-trace` at 1, so guest crashes reach the kernel log and with it the bug reports;
  0 and an `IOException("cannot start: …")` on failure), `boot`, `setRoutes`, `takeListens`,
  `takeKills`, `bklStall` (the latest too-long hold of the BKL as
  `<id>\t<seconds>\t<report>\t<end record or empty>`, tabs and line breaks in the last two turned
  into spaces, or null; it does not take the BKL), `takePower`, `processes`, `guestCpuTime`
  (live-plus-exited guest CPU nanoseconds under the BKL; −1 for a stopped handle or caught panic), `takeChangedDirs`,
  `trimMemory`, `shutdown`, `stop`.
- Consoles and processes: `openConsole`, `spawn` (negative console = the app's stdio), `read`,
  `write` (until all bytes are written), `resize`, `closeConsole`, `waitFor`, `consoleCwd`. The app
  itself no longer calls `read` or `resize` (the terminal's pump and resizes go through `vt.rs`);
  `MachineTest` does.

## Data flow / lifecycle

### Handles

- `start` builds `MachineOptions` (tier, exception trace), starts a `Machine` and registers it;
  every call clones the `Arc` under the table lock and releases the lock before working, so a
  blocked call keeps its kernel alive across a concurrent `stop`.
- `stop` removes the handle and then kills everything; `shutdown` runs the inittab shutdown
  (poweroff, 30 s) while the handle is still registered and then removes it. Both drop the removed
  `Arc` only after the table lock is released, since the last reference's drop (`kill_all`, sync,
  joining the timer thread, stopping the watchdog) can take long. Whichever thread finishes the
  last call on a removed kernel runs that drop.
- After the kernel is gone: `read` and `write` return −1, `waitFor` −9, `openConsole` and `spawn`
  throw "the machine is stopped", `takeListens`, `takeKills` and `takeChangedDirs` return empty
  arrays, `bklStall` and `consoleCwd` null, `takePower` and `processes` 0, `guestCpuTime` −1, and the rest
  (`boot`, `setRoutes`, `resize`, `closeConsole`, `trimMemory`, `shutdown`, `stop`) do nothing.
- Errors become `IOException`, or `FileNotFoundException` for every browse error whatever its
  errno, or sentinel values. `openConsole` and `resize` clamp rows and columns to 1–65535.
- `setRoutes` parses lines of `<hex destination>/<prefix> <hex gateway or -> <interface>` (later
  fields ignored); hex fields must be ASCII and even, the destination 4 or 16 bytes and the gateway
  the same length; the metric is 0 for IPv4, and for IPv6 1024 for a default route, else 256.
  Malformed lines are dropped silently.

### Callers

- All callers are JVM threads; the bridge attaches no native thread to the JVM, and guest and timer
  threads never call into Java. The main thread (`bklStall`, and `openConsole`/`spawn` from a tab's
  start), `tuxport-start` (`start`), the service's worker (the poll's calls, `setRoutes`,
  `consoleCwd`, `trimMemory`, `stop`, `shutdown`, the rules refresh's `spawn`), `tuxport-boot`
  (`spawn`, `waitFor`, `boot`), each tab's `vt-writer` (`write`) and `vt-waiter` (`waitFor`), the
  provider's binder threads (browse functions, `readFile`), and each tab's `vt-pump`, which reads
  the console through `Machine::read_console` in `vt.rs` and calls back into Java from inside
  `pump`.

### stderr and panics

- `init` redirects stderr once: fd 2 becomes a pipe read by a `stderr-log` thread that writes each
  line to logcat (tag `tuxport`, priority INFO; lines containing NUL are dropped); a failing `pipe`
  or `dup2` leaves stderr as it was. `verbose` sets `TUXPORT_LOG=1`.
- Panics are contained at two layers. The kernel wraps most `Machine` methods in `host_call`,
  which turns a panic into an error of the call when containment is on (`TUXPORT_ON_BUG=kill`, or
  by default whenever debug assertions are off, which is always on Android) and aborts the process
  otherwise. Everything else is caught by `guarded!`: `Machine::wait`, `processes`, `cpu_time_ns`, `take_power`,
  `stall`, `sync_store`, the Tier 1 pool trim, the wait loops of console reads and writes, the
  unwrapped parts of `boot`, `shutdown`, `kill_all` and `Machine`'s drop (their wait loops, flags,
  inittab plan parsing and thread joins; their signals, inittab reads, spawns and syncs run in
  `host_call`), and the JNI code itself, `vt.rs` included. `guarded!` runs the body in
  `catch_unwind`: the panic message reaches logcat through the panic hook's stderr, a
  line names the function, and the call returns its fallback (−1 for console reads and writes,
  `openConsole`, `spawn`, `browseOpen` and `browseOpenWrite`; 0 for `start`, `takePower` and
  `processes`; −9 from `waitFor`; null for objects; nothing for unit). The 17 `@Throws` functions
  also throw `IOException("internal error in …")` unless an exception is already pending. Kernel
  state a panic left half changed (a poisoned lock, a held BKL) is not repaired, so later calls
  may fail or wait.

### Application startup

- `MainApplication.onCreate` installs `Privacy` first, then `Diagnostics`. Manifest registration
  and provider configuration are in `android/build`.

### Guest browser client

- `android/build` packages `hosts/android/open-url.c` as the static `tuxport-open-url` asset
  for either distro; no guest package is needed.
- The client rereads `/etc/tuxport/browser`, connects to IPv4 loopback, sends token and URL lines
  and waits up to 5 s total for an `OK` reply. Invalid arguments/configuration, connection failures,
  refusal or a missing reply print an error and exit 1; URLs and tokens are never logged.
  Only HTTP(S) URLs up to 8192 bytes are accepted; the service performs strict UTF-8/URI checks.
  A timeout means no acknowledgment: it cannot prove no launch occurred or cancel buffered TCP requests.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`, `VtSession`, `ConsolePty`, `Rootfs`, `Documents` and `android/tests`
  call `Native`; `vt.rs` uses `guarded!`, `machine`, `string`, `throw` and the handle table.
- `android/build` compiles and packages this bridge and the guest browser client.

### Outbound

- `tuxport_kernel` (`Machine`, `MachineOptions`, routes, store and browse functions), `jni` 0.21,
  `libc` and Android `liblog`; terminal JNI also uses `libjnigraphics`.
- `MainApplication` installs `android/privacy` and `android/service` diagnostic capture.
- `android/service` receives the guest browser client's requests; `android/build` supplies its asset.

## Invariants and gotchas

- JNI symbol names hard-code `io.github.sajadxdp0.tuxport.Native` (and `VtNative` for the libghostty-vt
  terminal's functions); each `external fun` has exactly one `Java_…` function.
- The crate is `#![cfg(target_os = "android")]`, so it is empty on other hosts, where `build.rs`
  only prints its rerun lines.
- Every JNI function's body goes in `guarded!` (with `throws env` exactly when Kotlin declares
  `@Throws`); the macro is defined before `mod vt` so `vt.rs` can use it, and its closure moves the
  parameters, so a `return` inside returns from the body only.

## Confirmed bugs / technical debt

- Local references are never deleted in lib.rs's loops that build or read arrays (`string_array`,
  `takeListens`, `takeKills`, `strings`; harmless with ART's large table on every supported
  version); a failed string conversion with no pending exception makes void calls return silently
  (unreachable from the non-null Kotlin signatures). If the `stderr-log` thread fails
  to start, fd 2 is already the pipe and nobody reads it, so stderr writes block once it fills.

## Open questions

- Should browse errors other than a missing file (`EEXIST`, `EACCES`, `ENOSPC`) surface as
  `FileNotFoundException`?

## To verify

- The static browser launcher on Debian and a 16 KB-page device; current bridge tests use Alpine on API 34.
- ART's local-reference limit on API 30 and later.
