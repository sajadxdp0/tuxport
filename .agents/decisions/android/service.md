# Terminal service (android/service) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/service"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D6 were seeded by hand from the retired design
> doc §11.2 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Constraint index (compressed active decisions)

> Full text of D13, D17, D20: `git show ec9ad94b862479935821fbfad2affa7bbdba355e:.agents/decisions/android/service.md`

- **D13** — Preference writes use the androidx.core edit extension with asynchronous apply, as required by the lint build. (4c1a3ce)
- **D17** — Before every kernel start, including existing stores, call Rootfs.ensureContainersConfig after ensureNetworkConfig, write only a missing configuration, and fail startup if the store write fails. (0eb9e19)
- **D20** — The poll worker passes the same changed directories to Documents and Rootfs, ignores refresh failures and can delay the NOPASSWD refresh by a poll while the refresh process counts toward the service exit check. (d03c9b6)

## Active (full detail)

### D1: The kernel and all sessions live in a `specialUse` foreground service
- **What**: `TerminalService` owns the kernel handle and every terminal session; activities bind, register listeners and forward user actions; session events go to whichever listeners are present.
- **Why**: Destroying or recreating an Activity must not affect sessions or guest processes, and the foreground service keeps the app process from being treated as a cached process while the kernel runs.
- **Tradeoffs**: With a foreground service, Android 14 and later deliver no `onTrimMemory` callbacks, so memory release has to watch the UI itself (D4).
- **Source**: rounds 07 (`07-m5`), 187 (`187-app-init`)

### D2: The service outlives the last tab and stops only after an inittab shutdown
- **What**: Closing the last tab only finishes the Activity; the 2 s poll ends the service when there are no sessions, no UI and no guest processes; "exit all", guest `poweroff` and `halt` run the inittab shutdown before `stopSelf`; guest `reboot` starts a new kernel; boot runs on its own thread without blocking tabs.
- **Why**: Services started by OpenRC and `nohup` jobs should keep running without a tab, and exiting used to SIGKILL every process instead of letting them shut down.
- **Tradeoffs**: Nothing is shut down cleanly when the system kills the app; there is no autostart at Android boot, only on app cold start.
- **Watch out**: `exitAll` must clear the listeners immediately: a shutdown that finished before the Activity's `onStop` once looked like the user returning and restarted the kernel.
- **Source**: round 187 (`187-app-init`); design doc §11.2

### D3: Screen-off behavior is SLEEP, BUSY or ALWAYS independently of battery exemption
- **What**: Persist SLEEP, BUSY or ALWAYS with legacy false/true mapping and a remembered non-ALWAYS notification toggle; BUSY compares guest CPU every 2 s, releases after 30 s idle, protects transitions/downloads and renews grace for unavailable or delayed samples.
- **Why**: Idle terminals should save power while users can explicitly keep workloads running with a partial wake lock, independently of the optional battery exemption offered by android/terminal D69.
- **Tradeoffs**: Network waits and scheduled work may need ALWAYS, polling cannot wake a suspended device, and completed polls determine lock transitions.
- **Watch out**: Reject pre-reset samples and treat CPU changes rather than monotonic growth as activity; real-device suspend and power savings remain unverified, and repeated sampling delays prolong wakefulness.
- **Source**: round 191 (`191-keep-awake`); a3f7507e; 1e5de64e; 20137e4c; d053be24; 39e55b4b; 7a931d97

### D4: Memory is trimmed a minute after the UI disappears, and background tabs' images only under pressure
- **What**: When the last listener leaves, a 60 s timer runs `Machine::trim_memory` and then `VtNative.compressAll` (every tab's cold scrollback, android/terminal D42) in one worker task, and returning cancels it; `RUNNING_LOW` and above (except `UI_HIDDEN`) trim at once with `pressure = true`, which first has `VtNative.dropImages` cut the Kitty images of every started tab other than `current` (android/vt D3).
- **Why**: Android 14 and later send nothing a foreground-service process can act on; the delay avoids retranslating node or python after a brief look away; a tab whose output never pauses escapes idle compression; and losing images is acceptable only when memory is really short and not for the tab the user sees or comes back to.
- **Tradeoffs**: The full compression holds each tab's lock for its length and waits behind other worker tasks; on Android 14 and later the image cut never runs, so images stay until their tab closes; with the app in the background every tab but the last one shown loses its images.
- **Watch out**: Trimming and stopping share one single-thread executor, so a trim never runs on a released kernel; a `current` left pointing at a closed tab protects nothing, so every open tab is cut.
- **Source**: round 37 (`37-trim-memory`); 3f45ad0, c1a37da

### D5: After a kill, tabs come back on their saved screens without their processes
- **What**: A `running` marker detects an abnormal end; each running tab's login (an "auto" tab as the account `Rootfs.loginAccount` resolves at its first save, kept in `VtSession.resolvedLogin`), foreground cwd, plain-text scrollback (last 256 K, in transcripts named by their content) and, when its terminal changed, a deflated libghostty-vt snapshot (`s-*.snap`, named by content, android/terminal D44) are saved on the worker every minute, when the UI hides, on memory pressure and after tabs open, close or end, with `VtNative.version` in the index; on restart each tab gets a new shell in its directory below its restored screen (the snapshot, read at the tab's first show, else the transcript) and a notice, snapshots from another version are dropped, and saved tabs are kept until a killed run's tabs have been restored; a `name` key in an index from an earlier version is ignored.
- **Why**: v1 restores the layout, directories and screens but not programs, and says so; plain transcripts lost colours, images and layout; kills mostly happen in the background, hence the save on hide; unchanged files are not rewritten so the minute timer does not wear the flash; an "auto" tab resolved again at restore could become another user after accounts changed.
- **Tradeoffs**: Programs are not restored; each save may write both a transcript and a snapshot; a libghostty-vt upgrade sends tabs back to their transcripts once; tabs never shown since the restore keep their old files; names saved by earlier versions are lost at the next restore; an account added between a tab's opening and its first save (up to a minute) can still be the one recorded.
- **Watch out**: Old indexes load only because fields are looked up by name and extra keys are skipped, and no test reads such an index; the first save of each "auto" tab reads `/etc/passwd` through the store; an index naming a snapshot deleted by other means falls back to the transcript silently (logged); the sole index hash is `writtenLayout`, a nullable Int cleared under the Tabs lock.
- **Source**: round 194 (`194-restore-tabs`); design doc §11.2; 2af55e2, 89ad551; 7c35183; 3beb038; 600700b; 807e9ba1; 20137e4c; d053be24; 39e55b4b

### D6: Kernel kills are explained in the tab or a notification; Tuxport bugs always notify with a copyable report
- **What**: Each kill report goes to the tab of its controlling console as a yellow line, and to a notification when the UI is away, the program is full-screen or there is no tab; Tuxport's own bugs always notify, with "copy report" (app, device, distro, tier, the kernel's report line with panic location, recent kernel log records, and the unimplemented paths the killed process reached); OpenRC services that failed at boot are notified too, with notification detail and report copying subject to D28 privacy checks.
- **Why**: Tabs used to show only "Killed" or "Segmentation fault", which does not tell an OOM kill from an Tuxport bug; after release the copied report is what a user can bring to the developer.
- **Tradeoffs**: Reports are polled, so they arrive after the shell's prompt (hence the insertion above the cursor, `android/terminal` D2), and only kernel-bug reports can list unimplemented paths, since a killed run's crash and ANR reports have no kernel left to ask.
- **Watch out**: The unimplemented-paths section, read from the kill line's last field (android/native D17), was not exercised on a device or emulator.
- **Source**: rounds 195 (`195-kill-reports`), 205 (`205-copy-bug-report`), 207 (`207-report-kernel-log`); 99b4216; f61072f3; d053be24; 39e55b4b

### D7: The service is a module of its own, split from the app by responsibility
- **What**: android/service holds `TerminalService.kt`, `BrowserBridge.kt`, `Notifications.kt`, `Settings.kt`, `Tabs.kt` and `Diagnostics.kt`; the app is split into service, setup, documents, terminal and native modules by responsibility, in the maps only.
- **Why**: §11.2's lifecycle table had to leave the retired design doc, and the user asked for the app to be split.
- **Tradeoffs**: The code stays in one Gradle module, with kernel lifecycle in kernel/boot, sessions in android/vt, native mechanisms in android/vtcore, interaction in android/vtview and rendering in android/vtrender; tab rendering belongs to android/tabbar while session ownership remains here.
- **Watch out**: A shell's kill report can reach a tab opened within the next poll that took the freed console number (open in the map); the switch-after-failed-start crash, notifications posted after `onDestroy` and reports sent to ended tabs, recorded by the full refresh d9e1f0c, were fixed in 6560af6 (D8, D9, D16).
- **Source**: 12c4afa (bug list since updated by 89ad551, 3beb038, d9e1f0c, 6560af6); 0553229 (files, D23); 8e211142; 9015b244; c7309c0c, 8a17e4a9, 81f60bef; d053be24; 60790c14; 39e55b4b

### D8: The service holds `VtSession`s and releases them before the kernel stops
- **What**: `sessions`, `unshown`, `focus` and `current` are `VtSession`s; saving reads each session's console and login on the worker (no separate pty map); `removeSession`, `detach` and `onDestroy` release sessions (hang up and free the emulator) before the kernel is shut down or stopped; kill reports go to the tab whose console with that number is still open (`VtSession.consoleOpen`, android/terminal D8), else the newest tab that had it; title changes and ends arrive through `VtSession.Events` as `onSessionsChanged`; `releaseSessions` centralizes detach/onDestroy release, notification removal and clearing `sessions` and `unshown`.
- **Why**: The service held Termux `TerminalSession`s directly with a separate session → `ConsolePty` map, so the libghostty-vt terminal could not be one of its tabs (android/terminal D8).
- **Tradeoffs**: Listeners get no per-text-change callback; tabs redraw their view directly; a kill of a tab's own shell goes to a tab that took its console number before the next poll.
- **Watch out**: `ConsolePty.console` keeps its number after close, so routing must check `consoleOpen`; ended tabs keeping the service alive, saved tabs lost or paired with other tabs' transcripts and colliding kill notification ids were fixed in 89ad551 (D9).
- **Source**: 6ffb912; 89ad551; 6560af6 (reports to ended tabs, recorded in d9e1f0c); e35f9f7c

### D9: Exit all, restart and replace finish around an ongoing start instead of racing it
- **What**: While `starting` or `transitioning` is set, Exit all only marks and detaches and the start, restart or replace finishes the exit; the wake lock is held throughout, a download finished during a restart waits in `pendingReplace` until the new kernel is up or has failed, a failed start says so in the notification and stops the service once no UI is left and no switch is downloading (checked again when a download fails or is cancelled), `onDestroy` cancels a switch and sets `destroyed`, after which `updateNotification` posts nothing, and kill notifications take ids counting up from 100000.
- **Why**: Nothing tied the start thread, restart and replace to Exit all, the wake lock or a finished download, so an exit during a start left a kernel running after `onDestroy`, reboots dropped the wake lock and downloads finishing mid-reboot were lost.
- **Tradeoffs**: An exit during a distro replacement still imports the chosen distro first (and waits for a long import), a kill shown in a background tab now also notifies, and after a failed start the service and its notification stay without a UI while a switch downloads.
- **Watch out**: Every new path that ends `starting` or `transitioning` must check `exiting` (a map invariant), every new way of posting notification 1 must go through `updateNotification`, and kill notification ids from a new service instance can still collide with old ones after wrapping.
- **Source**: 89ad551; 6560af6 (a switch's completion crashing a destroyed service, recorded in d9e1f0c)

### D10: Every build honors the saved execution tier
- **What**: All builds read the saved tier with Tier 1 as the default, and `setTier` saves a changed choice and restarts the kernel while bug reports retain Tier numbers.
- **Why**: Release users need the same execution-mode control exposed by Settings.
- **Tradeoffs**: Tier 0 persists across launches until changed.
- **Watch out**: A Tier 0 preference saved by an earlier debug installation now applies in release builds.
- **Source**: 62a8c07; cf4f5e20; 20137e4c; d053be24; 39e55b4b

### D11: Display settings the terminal needs before binding live in the service's `settings` file
- **What**: `Settings` keeps `fill_wide` defaulting off and `volume_keys_switch_tabs` defaulting on, preserves saved values, and exposes plain reads while service setters save and notify listeners; MainActivity rereads fill_wide on start and volume behavior on each press.
- **Why**: Settings rows display and forward state the service keeps, and the terminal's activity needs the value before it binds the service.
- **Tradeoffs**: The service owns display preferences it does not use, unsaved CJK enlargement now defaults off, and each volume press reads an in-memory preference.
- **Watch out**: MainActivity must reread fill_wide after returning from Settings, and writes bypassing the service skip listeners; existing explicitly saved true values still enlarge wide glyphs.
- **Source**: 0478dd5; 0553229 (read through `Settings`); 61df6915; 75123eef; 20137e4c; d053be24; 39e55b4b

### D12: `TerminalService` reads in the order a service runs
- **What**: The service orders types/state, creation and start helpers, commands/binding/listeners, sessions/saving, reports/routes, memory/settings, distro replacement and shutdown; start dispatches guarded main-thread outcomes to `started` or `startFailed`, while Tabs retains its lifecycle order.
- **Why**: Lifecycle helpers had drifted from callers and nested start outcomes obscured the start/exit guard.
- **Tradeoffs**: Callback objects stay among properties, and helpers can remain far from secondary callers.
- **Watch out**: Kotlin initializers must keep safe textual order, and `started`/`startFailed` must run only after start’s main-thread post clears `starting` and checks `exiting`.
- **Source**: a546fea; 4de8aacd; e35f9f7c; 20137e4c; d053be24; 39e55b4b

### D14: A restored run says why it ended, from Android's exit record
- **What**: `markRunning` writes the pid into the marker and `takeKilled` returns it with the saved tabs; the service looks up `getHistoricalProcessExitReasons(package, pid, 1)` and opens the restored notice with one of seven reasons (low memory, background resources, force-stopped, updated, crash, ANR, general); a crash or ANR also posts notification 3 on the problems channel with "Copy report" (D6's header plus the pid, the kind, the UTC time and Android's ANR description), even when no tabs could be read.
- **Why**: Every killed run was explained as "Android stopped the app", so an Tuxport crash looked like the system's doing and produced no report.
- **Tradeoffs**: The pid ties the record to the run, since the newest package record could be a provider-only process started later; a marker without a pid or a record Android no longer keeps falls back to the general text; the report holds only what the exit record holds (no Java stack trace or native tombstone); updates are named only from Android 14.
- **Watch out**: Low-memory kills are told apart only where `isLowMemoryKillReportSupported`; low memory, background resources and ANR were not triggered in testing, and the Chinese strings were not seen on a device; retained diagnostic evidence is now collected separately under D29.
- **Source**: a1f47cc; c7309c0c; d053be24; 39e55b4b

### D15: Bug reports name the app Tuxport; the in-tab tag stays `[tuxport]`
- **What**: The copied report's first line is "Tuxport <version> (<code>, <build type>)", while the "[tuxport]" tag before restore notices, kill reports and the notice that tmux control mode was detached (android/terminal D49) stays lowercase, as a source tag like "sudo:".
- **Why**: The report opened with the product name in lowercase.
- **Tradeoffs**: A notice can read "[tuxport] ... because Tuxport crashed", mixing both casings in one line.
- **Watch out**: Capitalising the tag means changing TerminalService's two `showMessage` calls together with the setup module's "tuxport:" prefixes (android/setup D16).
- **Source**: 3f04bf5 (with android/terminal D28); 77d7f36 (the tmux notice)

### D16: A launch without a ready store resumes the import of the recorded distro
- **What**: Without a ready store `startOrImport`, shared by `onCreate` and the restart that finishes Exit all, calls `start(Rootfs.importing(this))`, the distro the interrupted import recorded (android/setup D17), and the existing fallback to Alpine with `switch_failed` covers a resumed import that cannot download or import.
- **Why**: The service imported Alpine whenever no store was ready, so a switch interrupted by a kill silently became Alpine, and `finishExit` restarted with `start(null)`, which never imports.
- **Tradeoffs**: A resumed Debian import needs the network again when its tarball is gone from the cache.
- **Watch out**: None known.
- **Source**: 237d663; 6560af6

### D18: A long BKL hold is reported in a notification while it lasts
- **What**: The 2 s poll first calls `Native.bklStall` (android/native D18) on the main thread; a new stall of the current kernel posts notification 4, "Tuxport's kernel is stuck", with its seconds and "Copy report", and once it has ended a notification still showing becomes "was stuck" with the total and the kernel log record in its report, without alerting again, while a dismissed one stays dismissed.
- **Why**: A stuck kernel left the user with frozen tabs and no explanation or report to send.
- **Tradeoffs**: A later stall replaces an earlier notification, and a stall that ended before any poll saw it is still reported once.
- **Watch out**: `bklStall` is an exception to the map's rule that every BKL-taking native call goes through the worker, so it must never start taking the BKL; whether the notification should update its seconds during a stall is open in the map.
- **Source**: 64a6107 (with kernel/process D38)

### D19: Bells and OSC 9/777 notifications reach the user per tab; clipboard writes go through the service
- **What**: A bell from the tab in front (`current` with the terminal Activity listening) goes to the listeners' `onBell`; otherwise bells, at most one per second per tab, and OSC 9/777 notifications become one notification per tab on a new `terminal` channel (ids from 200000 plus the tab's id), titled by OSC 777's title or the program's title with "Tab N" as subtext, opening that tab and removed when it is shown or closed; OSC 9/777 from the tab in front invokes onBell with its 100 ms throttle and touch-feedback setting, while new terminal channels enable vibration and both terminal and problems channels request high importance under their existing IDs; clipboard writes from any tab that passes the privacy gate set or clear the clipboard, with a toast only before Android 13, and clipboard reads from the tab in front go to the listeners' `onClipboardRead` while reads from other tabs are answered empty.
- **Why**: Bells and notifications matter most when the user is not looking at the tab, which only the service knows, and the clipboard needs a context that outlives the Activity.
- **Tradeoffs**: Foreground feedback uses the bell REJECT pattern and existing channels retain their settings; the system may bundle tab notifications with the ongoing one, so the first tap expands the bundle; on Android 13 and later, background tabs allowed by the privacy gate can overwrite the clipboard with only the system's own confirmation, and a background tab reading the clipboard gets nothing.
- **Watch out**: Existing channels keep user settings and heads-up alerts depend on Do Not Disturb and vendor behavior. Physical feedback depends on device settings and remains unverified, a bell flood still posts to the main thread, and per-tab notification limits allow several tabs to notify independently.
- **Source**: d89b5fa (with android/terminal D36); 600700b (clipboard reads); f61072f3; d053be24; 2ff6f22c; 39e55b4b; a13a8b98

### D21: Program statuses and finished commands notify through the tab's notification
- **What**: `notifyStatus` posts the tab's notification (D19's) once per status for blocked, done and error, titled by the record's or the tab's title with the status in words and its message, and withdraws it when the status goes without the user unless a bell or OSC 9/777 replaced it (`statusNotified`); `onCommandFinished` posts it for a command of at least 10 s in a tab not in front ("Command succeeded", "Command failed" or "Command finished", the time taken in ICU's short units and a failure's exit code), replacing a status notification; in the tab in front done and error count as seen at once, and listeners hear `onTabStatus`.
- **Why**: A phone shows one tab at a time, so a program waiting for the user, finishing or failing, or a long command ending in another tab or with the app away, goes unnoticed, and only the service knows which tab the user sees.
- **Tradeoffs**: Statuses, finished commands, bells and OSC 9/777 share one notification per tab and replace each other; no setting turns command notices off, and their text cannot name the command; the record's app name is never shown.
- **Watch out**: CLIs that send both OSC 9 and 7501 for one event alert twice, the withdrawal relies on every other path that posts the tab's notification clearing `statusNotified` first, and command notices depend on each shell’s prompt integration (android/setup D20 and android/vt D5–D7).
- **Source**: 22c8ab0, 5256ae2; a7d8c9f5; a809c5a8; 7aa817af (prompt integration)

### D22: The service keeps the cursor settings and gives them to every session
- **What**: `cursor_style` (Ghostty's names block, bar and underline) and `cursor_blink` live in the `settings` preferences; `setCursorStyle` and `setCursorBlink` save them and call `cursorChanged`, which sets every session's values and tells listeners, and `createSession` gives each new or restored session the current ones (android/terminal D47).
- **Why**: The cursor setting must reach every tab, background and restored ones included, which a static read by `MainActivity` for the shown view, as for `fill_wide` (D11), would not.
- **Tradeoffs**: A change sets every tab's terminal under its lock on the main thread.
- **Watch out**: A session not started yet only stores the values until its terminal exists.
- **Source**: 77d7f36

### D23: Notifications and settings live in classes of their own; the service keeps the decisions
- **What**: `Notifications(service)` owns the channels, every notification id but the ongoing one (1), the stall, crash, boot-failure, listen and kill notifications with the bug report and its "copy report" action (the service hands it the intent), and each tab's notification with the bell throttle, status de-duplication and stall state, while the service still decides what is told (the tab in front, a kill shown in its tab); `Settings` is an object over the `settings` preferences with getters, plain setters and `clearLogin`, whose values the service's getters delegate to and its setters write before acting, with file_access replacing the old system_documents contract (D27); saving tabs and switching distros stay in the service, coupled to its start and worker.
- **Why**: TerminalService mixed the kernel's lifecycle, sessions and saving with notification building (channels, ids, "copy report", per-tab de-duplication) and the preferences' keys and defaults, and exported static settings only because the preferences lived there.
- **Tradeoffs**: A notification change can need both files (the decision in the service, the text in Notifications), and settings have two entry points: `Settings` for reading anywhere and the service for changes with effects.
- **Watch out**: Notification ids are split between the service (the ongoing 1) and Notifications' companion, so a new id must be checked against both, and writing a setting through `Settings` instead of the service skips its effects (wake lock, Files roots, cursor to every tab, restart).
- **Source**: 0553229; d0ddf170; d053be24; 39e55b4b

### D24: Notification localization follows configuration changes

- **What**: Shared builders create non-ongoing notifications, ICU formats failed-service lists, and changed channel locales trigger channel renaming and reposting the ongoing notification while bug reports stay English.
- **Why**: Repeated builders and English separators drifted, and running services kept old notification/channel languages.
- **Tradeoffs**: Already posted notifications retain their old language, and Notifications remembers channel locales.
- **Watch out**: The service must see updated resources in onConfigurationChanged; the locale-change path was not exercised on a device.
- **Source**: aed99afb; 20137e4c; d053be24; 39e55b4b

### D25: Mirror probing runs only after import and is joined before replacement

- **What**: The service launches a silent post-import mirror probe, removes manual mirror/measuring APIs and still joins the probe before deleting the store.
- **Why**: Import-only mirror selection no longer needs Settings choices or progress callbacks.
- **Tradeoffs**: Probe outcomes appear only in logs.
- **Watch out**: The probe can still overlap the first kernel store open, with the existing possible skipped boot cleanup.
- **Source**: a1367531; 20137e4c; d053be24; 39e55b4b

### D26: A kernel owns an authenticated loopback browser endpoint

- **What**: Each kernel gets a bounded authenticated loopback endpoint that dispatches browser requests to the focused UI and closes on failure, detach or service destruction.
- **Why**: Multiplexer clients need browser access independent of terminal escape forwarding or tab identity.
- **Tradeoffs**: Every guest process able to read the configuration shares the capability, and requests carry no originating tab.
- **Watch out**: Isolated guest network namespaces cannot reach the endpoint, and a slow client can delay its serial worker.
- **Source**: 8e211142; d053be24; 39e55b4b

### D27: External file access requires a fresh default-off opt-in

- **What**: Settings.fileAccess defaults false, ignores system_documents and is forced false during privacy mode; service changes save the gate, revoke provider grants when disabled and notify roots and listeners.
- **Why**: The former visibility preference cannot authorize the broader exposure of both document roots.
- **Tradeoffs**: Users must opt in again, and enabling privacy clears the saved choice without restoring grants afterward.
- **Watch out**: Writes bypassing the service miss revocation or notifications, and already-open descriptors and copied data remain outside revocation.
- **Source**: d0ddf170, f61072f3 (android/documents); d053be24; 39e55b4b

### D28: Privacy redacts notifications and authenticates report copying

- **What**: Privacy mode shows the privacy title and selected running policy on the ongoing notification, hides session, event and startup-error details, clears old detail notifications and guards guest clipboard access, while ordinary reports use a non-exported receiver and private reports a non-exported authentication activity with a delivery-time lock check.
- **Why**: Background workloads should continue without exposing private content through notifications or clipboard callbacks.
- **Tradeoffs**: Exit, cancellation and running-policy controls remain available without authentication, old detail notifications are discarded, and copying a report need not start a kernel.
- **Watch out**: Keep legacy delivery checks aligned; receiver clipboard delivery and report-specific authentication still need device-variant coverage.
- **Source**: f61072f3; d053be24; 39e55b4b; c7e338a5, c97b00bf

### D29: Diagnostics retain bounded evidence and export independently of the kernel worker

- **What**: Diagnostics keeps own-PID logs and fault reports for the current and previous runs within 2 MiB, exporting metadata, exit records and bounded traces while isolating pending saves from the three-archive share cache.
- **Why**: Troubleshooting evidence must survive failure and notification dismissal.
- **Tradeoffs**: Continuous capture adds bounded I/O and storage, and older records or oversized traces are discarded.
- **Watch out**: Process death can lose final logcat messages, vendor traces vary, and retained messages can contain names and paths.
- **Source**: c7309c0c; d053be24; 39e55b4b

### D30: Tab-list visibility is a persistent service setting

- **What**: show_tab_list defaults to true, persists in shared settings and notifies activity listeners through service setters.
- **Why**: The visibility choice must survive activity and process recreation.
- **Tradeoffs**: The service stores a display preference that it does not consume.
- **Watch out**: Direct settings writes bypass listener notification.
- **Source**: 5c49c5a7; 39e55b4b

### D31: The ongoing notification puts a startup failure reason in the body

- **What**: Ordinarily the title names the current state and the body names the running policy, but a non-private startup failure shows Cannot start above the error in both body layouts.
- **Why**: State titles avoid repeating the app name and the expanded body gives long errors room.
- **Tradeoffs**: A public startup error displaces the policy text, while privacy mode shows the privacy title and policy even on failure.
- **Watch out**: Preserve exit and progress precedence and check long text and vendor layouts on devices.
- **Source**: c7e338a5, c97b00bf

## Superseded

- ~~A `Session` interface in the service, with `createSession` building a `TermuxSession` or a `VtSession` by `VtNative.available` (6ffb912)~~ → narrowed to `VtSession` only by android/terminal **D8** in b38d29c (2026-11-04)
- ~~A tab's name among D5's saved fields, saved again after each rename (2af55e2, 89ad551)~~ → dropped from **D5** in 7c35183 (2026-11-05), with renaming gone from the terminal
- ~~The trim releases only the kernel's caches (D4, round 37)~~ → replaced by **D4** in 3f45ad0 (2026-11-10)
- ~~Restored tabs show their old scrollback as plain text, losing colours (D5's tradeoff, round 194)~~ → replaced by **D5** in 600700b (2026-11-10, no SUPERSEDES declared)
- ~~Settings read across modules through the static `TerminalService.fillWide(context)`, `login()` and `showSystemDocuments()` (D11, 0478dd5)~~ → replaced by **D23** in 0553229 (2026-11-12)
- ~~D3’s boolean keep-awake preference and notification toggle~~ → replaced by **D3** in 1e5de64e (2026-11-13)
- ~~D10’s release-only Tier 1 enforcement~~ → replaced by **D10** in cf4f5e20 (2026-11-13)
- ~~D11’s default-on fill_wide preference~~ → replaced by **D11** in 75123eef (2026-11-13)
- ~~D12’s former ordering of settings followed directly by exit and stop paths~~ → replaced by **D12** in 4de8aacd (2026-11-13)
- ~~D23 system_documents getter/setter contract from 0553229~~ → replaced by **D23 and D27** in d0ddf170 (2026-11-14)
- ~~D19 dropping foreground OSC 9/777 requests~~ → replaced by **D19** in 2ff6f22c (2026-11-15; background routing remains unchanged)
- ~~No exemption request anywhere in the app under D3~~ → narrowed by **android/terminal D69** in 7a931d97 (2026-11-16, implicit cross-module change).
- ~~The generic privacy body under D28~~ → replaced by **D28 and D31** in c7e338a5 (2026-11-17).
- ~~Always showing the policy body for a non-private startup failure in c7e338a5~~ → replaced by **D31** in c97b00bf (2026-11-17, recovered headingless block).
