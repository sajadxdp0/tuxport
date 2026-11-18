# android/tests Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/tests.md` for the paired decision history (not yet created; pre-split decisions D5, D8, the test portions of D9, D12–D14, D16, D19 and D23–D29 stay in `.agents/decisions/android/native.md`).
>
> Verified: 2026-11-15 (split out of the android/native full mapping of 2026-11-10; the boundary was checked against the source at the split)

## Responsibilities

`android/tests` owns Android instrumentation source in `android/app/src/androidTest/`: kernel
bridge integration, guest setup, terminal interaction, privacy, documents, notices and diagnostics
checks. Runtime behavior belongs to the corresponding Android and kernel maps; Gradle runner,
debug variant, manifest and network-security configuration belong to `android/build`.

## Key types

- `MachineTest` — kernel and app instrumentation tests; `RunPolicyTest` — isolated preferences and idle-grace tests;
  `LicenseTest` — static notice coverage and offline reader rendering/restoration, without opening the store.
- The focused test classes and their isolation boundaries are listed below.

## Public entry points

- From `android/`: `./gradlew connectedDebugAndroidTest`.
- `-Pandroid.testInstrumentationRunnerArguments.network=true` enables network-dependent cases;
  `screenshots=true` saves the paste-dialog preview.

## Data flow / lifecycle

### Tests

- `TabListTest` checks session identity across removal/rebinding, a bounded, scrollable 50-tab list
  and preserved editor focus during opening/scrolling/dismissal; all three cases passed on API 34.

- `ShellTitleTest` uses separate stores for two cases: real bash/ash title restoration after
  a program exits, directory changes and Unicode/control paths; bash also covers command titles,
  pipelines, literal/control text, history fallback, exit-status preservation and hook reloads.
  These and the two existing prompt-mark/login tests passed on API 34.

- `TerminalResizeTest` uses isolated stores and shell-free terminals for seven API 34 checks:
  preview pixels under full-frame requests and subrow layouts, split control sequences, quiet
  output, timeout, complete repaint and detach cleanup. It drives layout/draw directly, not the IME.
- `TerminalFilesTest` uses isolated stores for binary/empty imports, collisions, name/home
  escaping, read/close failure cleanup and missing homes; a real guest shell checks reads and
  clipboard quoting. A locked MainActivity checks pending-picker retention across recreation;
  all seven cases passed on API 34.
- `DiagnosticsTest` checks two-part byte limits and previous-run rotation, ZIP entries and
  report retention, own-PID capture excluding another process, read-only share intents and
  provider root confinement, plus rejection of locked/stale export callbacks and recreation.
  Save coverage includes ViewModel ownership across recreation, one-shot consumption and owner
  cleanup, plus isolation from share pruning and FileProvider access.
  It uses API 34 and does not bypass biometric authentication.
- `TerminalInputTest` uses an isolated store and native terminal, a shell-free session and a
  capturing view host to exercise IME replacement/commit/composition and clipboard menu callbacks.
  It checks confirmation and cancellation, no delayed replay, bracketed paste and its terminator,
  ordinary Chinese composition/Enter, and trailing-newline removal without trimming other
  whitespace (including unsafe/empty remainders and changed terminal modes) on API 34. The
  `screenshots=true` runner argument saves `cache/paste-preview.png` for visual inspection of
  the terminal-themed dialog; the tests do not drive Gboard's UI.
- `PrivacyTest` covers default-off/mutually exclusive file access, cold and recreated locked pages, pause-only visibility versus stop-and-return locking, secure dialog flags, non-exported action targets, service/broadcast/activity routing and locked-report clipboard denial without an authentication bypass. `MachineTest.documentsShowTheHomeDirectory` also checks every provider denial with privacy on and a stale true file-access preference.
- `BrowserTest` imports separate temporary stores and checks custom launcher preservation,
  launching without a TTY or stdio, root and `su` users, endpoint replacement with a surviving
  shell, authentication and URL validation, refusal/timeout, queued UI cancellation, and Custom Tab intents.
  Its `network=true` test installs tmux/screen and checks reattach on another console and nesting.
  It does not perform a real OAuth login.
- `RunPolicyTest` uses a separate preference file for legacy migration and remembered toggle
  targets, and checks idle expiry, activity, failed samples and delayed delivery with a fake clock.
  `MachineTest.guestCpuTimeTracksRunningAndExitedThreads` checks both tiers, exited and pooled
  threads, idle stability and stopped handles.
- `MachineTest` runs in the debug app's own process against the app's real store and
  preferences: `@Before` imports the bundled rootfs when no store is ready, `start(tier)` records
  each handle and `@After` stops them; tests change guest files, accounts and preferences and put
  them back in `finally`. It covers both tiers (guest page size 4096), calls after stop, memory
  trim, boot and shutdown by inittab (under 10 s), osclock, consoles (input, Ctrl-C, concurrency,
  the foreground cwd), login from `/etc/passwd`, the saved directory, accounts and doas (two logins installing it at once, and one waiting for apk's lock, included), NOPASSWD
  rules following a password set later, saved tabs, the Files provider (default-off access including
  legacy preferences, disabled queries/reads/changes and file links, authority-wide grant revocation,
  home, no home, writes,
  changes both ways, grants below a directory as a pure check of `Documents.grantedUnder`), manual
  mirrors, verified, resumed and cancelled downloads (`file:` URLs and a local server), routes, low
  ports, bugs injected through the test hooks killing only the guest process on both tiers, kill
  reports (including an OOM from a cgroup `memory.max`), a guest crash reaching the kernel log, and
  prompt marks (busybox ash's with root's shell set to `/bin/sh` for the test), and a
  new Alpine import logging in with bash (`newSystemsLogInWithBash`, its own store: the offline
  install, the `/bin/sh` fallback, bash's marks and nested shells'). `newSystemsInstallProcpsOffline`
  uses its own store and ash, omits a dependency to check failure/retry, retries in two concurrent
  logins without repositories, runs ps/top/free/vmstat/uptime/pgrep, and checks that user removal
  persists. `newSystemsInstallBubblewrapOffline` similarly uses its own store without bash/procps
  or repositories, checks a missing libcap dependency, concurrent retries and user removal, and
  runs root and unprivileged sandboxes with namespaces, read-only root and private tmpfs.
  Only `downloadedDistributionsRun` (about 50 MB of Debian, its own store) downloads
  from the internet, behind the `network=true` argument; `routesComeFromLinkProperties` has no
  gate but fails without a default network with an IPv4 gateway.
- The hooks need the debug variant (`test-hooks`) and `TUXPORT_TEST_HOOKS=1`, which the tests set
  and never unset. The KDoc and README ask for runs on 4 KB and 16 KB-page arm64 emulators; CI does
  not run the tests (they need KVM). Nothing here covers `Native.bklStall` (the stall itself is
  tested in `hosts/cli`), terminal gestures, real IME UI or persisted grants end to end.

## Dependencies (inbound / outbound)

### Inbound

- Developer/device test runs through `android/build`'s instrumentation runner; CI does not run these tests.

### Outbound

- `android/build` supplies the debug APK, test hooks, assets and local-server security configuration.
- `android/native` exposes real kernel/store operations; the tests exercise the corresponding
  `android/*` runtime modules and guest programs, with androidx.test runner/ext.junit.

## Invariants and gotchas

- The instrumentation tests are not isolated from the app: they use its store, preferences and
  saved tabs (a store switched to another distribution breaks the Alpine assertions). Root logs
  in with bash on a store this version imported and with busybox ash on an older one, so
  assertions on a command's output in a login go through `plain`, which drops control sequences
  and CRs (bash puts its marks and bracketed paste right before the output).
- Tests that require test hooks must use the debug build; successful API 34 runs do not
  establish physical-device, real keyboard or 16 KB-page behavior.

## Open questions

None.

## To verify

- Diagnostic exports passed six instrumentation cases and seven privacy cases on API 34;
  debug/release assembly and lint passed. Manual checks covered authentication before picker
  launch, saved ZIP integrity, picker cancellation cleanup, locked return without another prompt,
  and process-death orphan cleanup. Real biometric devices, API 30 trace attachments
  and 16 KB-page device behavior remain unverified.
- The static browser launcher on Debian and a 16 KB-page device; current bridge tests use Alpine on API 34.
- The complete test suite on a 16 KB-page image today.
