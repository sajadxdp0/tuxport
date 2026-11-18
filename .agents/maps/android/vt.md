# android/vt Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/vt.md` for the paired decision history (it still holds the pre-split decisions for `android/vtcore`; earlier decisions remain D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49 in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-11 (split out of the android/terminal full mapping of the same day: 5 research concerns, 18 claims checked by 3 independent verifiers; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: split native terminal ownership, JNI and emulator behavior into android/vtcore, retaining Kotlin sessions and consoles here)

## Responsibilities

`android/vt` owns the Kotlin session of each terminal tab and its kernel console:

- opening the console and login shell lazily, starting the pump, writer and waiter threads,
  resizing, waiting for exit and closing exactly once;
- the session's progress and status records, pending clipboard reads and main-thread events to
  the service, including command completion and tmux-detach notices;
- selecting a snapshot or transcript when restoring, keeping the last transcript for saving,
  pending messages, and the session-side clear/reset and cursor-setting calls.

Root: `VtSession.kt` and `ConsolePty.kt` in
`android/app/src/main/java/io/github/sajadxdp0/tuxport/`.

Outside the module: `VtNative`, JNI, the pump implementation, terminal ownership and locks,
protocol parsing, command timing, snapshots, reset semantics and compression (`android/vtcore`);
view lifecycle and interaction (`android/vtview`), frame encoding and drawing (`android/vtrender`),
the surrounding screen (`android/terminal`), session ownership and persistence policy
(`android/service`), the kernel console (`kernel/tty`), kernel JNI (`android/native`), and the
login script and prompt-mark producers (`android/setup`).

## Key types

- `VtSession` — a tab owned by the service: its `id` (from the service, naming the tab in a
  notification's intent), its `ConsolePty`, the terminal handle (0 until the shell starts, never
  reset), the view showing it (`view`, read on the pump thread), a restored tab's snapshot (a
  loader the service gives) and transcript, the pending notice and messages, its login and the
  account an `auto` login resolved to (`resolvedLogin`, written by the service), the file of its
  last saved snapshot (`savedSnapshot`, used by the service's worker only), the last transcript
  (its last 256 K characters, also returned while the screen is unchanged), the last progress
  report (`progress`, a
  `Progress`), the program status records by id (`Status`, least recently updated first, at most
  64) and the one most in need of the user (`status`), the clipboard reads waiting for an answer
  (the clipboard each named), a `vt-pump` thread, a `vt-waiter` thread and its own single-thread
  `vt-writer` executor, the cursor's default shape and blinking (`setCursor`, from the service);
  `Events` (title changed, bell, clipboard write, notification, clipboard read, progress or status
  changed, a command finished, tmux's control mode detached, the shell finished) go
  to the service on the main thread; `busy` (closing it would end something: the shell runs, has
  started and is not at its prompt) and `cwd()` (its foreground group's directory, from the
  kernel). Its companion holds
  libghostty-vt's progress states, status states and kinds, and `describe` (a status in words).
- `ConsolePty` — `start` (open a console, spawn `login.sh` through `sh -c` with the tab's login
  and cwd and a fixed environment, `TERM=xterm-256color` among it), `waitFor`, `close`; `console`
  is −1 until started and keeps its number after `close` (`VtSession.consoleOpen` tells whether it
  is still open, for the routing of kill reports in android/service).

## Public entry points

- Lifecycle: `VtSession.start`, `resize`, `write`, `finishIfRunning`, `release`;
  `ConsolePty.start`, `waitFor`, `close`, `script`, `argv` and `ENVIRONMENT`.
- State and saving: `tab`, `view`, `console`, `consoleOpen`, `login`, `resolvedLogin`, `id`,
  `title`, `isRunning`, `busy`, `cwd`, `setRestored`, `transcriptText`, `takeSnapshot`,
  `savedSnapshot`, `showMessage`, `setCursor`, `clear` and `reset`.
- Program reports: `progress`, `status`, `statusSeen`, `describe`, `answerClipboardRead`,
  and `Events`; native pump callbacks `onOutput`, `onTitle`, `onBell`, `onClipboard`,
  `onNotification`, `onProgress`, `onStatus`, `onClipboardRead`, `onCommandFinished`,
  `onTmuxDetached` and `onForeground`.
- Calls run on the main thread except the native pump callbacks and the saving fields/methods
  used on the service's worker. Callbacks post service events to the main thread; `onOutput`
  invalidates the attached view from the pump thread.

## Data flow / lifecycle

### Start and end

- A `VtSession` starts its shell the first time a view showing it knows the grid (`VtView.attach`
  and every regrid call `Host.onGrid`, which starts the current tab once): `ConsolePty.start`
  opens a console of the grid's size and spawns the login. `createTerminal` tries a restored
  snapshot through `VtNative.restore`, otherwise makes a terminal with `VtNative.create` and
  prints the restored transcript when present. Both use android/vtcore's terminal options.
  The session applies the user's cursor shape and blinking immediately after creation, before
  the pending notice and messages; then it starts the writer, pump and waiter.
- If opening the console or spawning fails, the reason is printed, the console is closed and the
  tab counts as ended (127), with no "Process completed" line and no writer. If the terminal cannot
  be made (the kernel is gone, libghostty-vt fails, a panic), the tab ends (127) without a word,
  and later messages for it are refused.
- A restore replaces whatever was pending with its notice; a message shown before the terminal
  exists is queued after it as a plain yellow line, not inserted as below.
- The `vt-pump` thread runs android/vtcore's `VtNative.pump`, including while no view shows the
  tab. It reads the console, feeds the terminal and delivers callbacks; the session invalidates
  its view and posts events to the service. The native implementation owns reply writes,
  protocol parsing and termination conditions.
- The waiter waits for the shell (−9, "signal 9", once the kernel is gone), gives the pump up to
  0.5 s to take the last output, then on the main thread closes the console (once), stops the
  writer and, unless the tab was released, drops the tab's progress and status records, prints
  "[Process completed (code N | signal N) - press Enter]" and reports the end.
- `release` (the tab closed, or its kernel going away) hangs up the console once, stops the writer
  (writes already queued still run) and destroys the terminal handle; a running pump keeps the
  terminal until it returns.

### Clipboard, progress and status

- Clipboard reads, as Ghostty's default `clipboard-read = ask`: the session queues them (the
  clipboard each named) and tells the service once for the reads that come before an answer; the
  user is asked in the tab in front (android/terminal), and `VtSession.answerClipboardRead` writes
  one answer per read through the tab's writer, in Ghostty's form `ESC ] 52 ; <clipboard> ; <base64>
  ESC \` (ST, whatever ended the request): the text when allowed, nothing (an empty clipboard, as
  libghostty-vt answers a read it refuses, so the program does not wait for a timeout) when denied,
  dismissed or asked from a tab not in front.
- Progress and status (`VtSession`, main thread): a progress report replaces the last one (a
  removal clears it) and lasts 15 s without another, as in Ghostty. Status reports of a read are
  applied together, as the specification keeps them: one record per id, a report replacing its
  record whole, a clear removing the record and those beneath it (`build` takes `build/test`; an
  empty id, as after a full reset, takes all), at most 64 records, the one updated longest ago
  dropped; text direction controls are removed from titles and messages. `status` is the record
  most in need of the user: blocked, then error, done, working, idle, the most recently updated
  among equals. A change of either (by value) is reported to the service (`onStatusChanged`).
- The pump stands in for shell integration's prompt mark, which only some shells send: after a
  read that carried a progress or status report it notes the console's foreground process group
  (`Machine::console_foreground`; a failed lookup notes none, and that report then waits for its
  timeout, being seen or the shell's end), and on a later read where another group (or none) has
  the foreground it calls `onForeground`,
  which drops the progress and the working, blocked and idle records, keeping done and error ones
  until seen. Seeing the tab (`statusSeen`, from the service) drops done and error records; the
  shell's end drops everything.
- Native command timing and tmux control-mode detection arrive as `onCommandFinished` and
  `onTmuxDetached`; the session posts them to `Events` unless released. The service decides
  which notices to show. Parsing and the tmux detach itself belong to android/vtcore.

### Saving, messages and cursor actions

- Saving: `transcriptText` formats the primary screen with its scrollback as plain text (soft
  wraps joined, trailing blanks trimmed) and returns its last 256 K characters
  (`Tabs.MAX_TRANSCRIPT`, all that saving keeps), which it also keeps; on the alternate screen,
  when nothing changed the screen since the last transcript (no output, message, resize, clearing
  or reset), or when formatting fails, it returns the last transcript taken on the primary one (""
  when none was). The service also takes the terminal's snapshot (android/vtcore's Snapshots and restoring).
- `setRestored` keeps the transcript, lazy snapshot loader and notice until the first start.
  `takeSnapshot` forwards android/vtcore's three-way result: null before start or when no new
  snapshot exists, empty when none can be taken, bytes for a new snapshot. `Tabs` owns version
  checks, files and deflation; `savedSnapshot` identifies the file to keep on a null result.
- `showMessage` queues a plain yellow line before the terminal exists (unless creation already
  failed); afterwards it calls `VtNative.message`, which refuses the alternate screen and inserts
  the notice above the cursor line. The insertion and parser recovery are android/vtcore's.
- `resize` ignores an unchanged grid/cell size, calls `VtNative.resize` for terminal reflow and
  the console's SIGWINCH, and sends its replies immediately through the writer. `write` queues
  input on the tab's executor only while the shell is running.
- `clear` calls `VtNative.clear`: -1 leaves an alternate screen unchanged; 1 writes a form feed
  (^L) for the shell to redraw its prompt; 0 needs no form feed. `reset` similarly sends ^L
  when `VtNative.reset` says the cursor was at a prompt, invalidates the view and reports a title
  change. Neither session action drops progress or status records; the native screen/mode
  semantics are in android/vtcore's Clearing and resetting.

### Shell integration

- `busy` is false before the terminal exists or after the shell exits; otherwise it negates
  `VtNative.atPrompt`. At-prompt means the cursor is in the primary screen's marked prompt or
  input, so a typed command counts as busy after Enter until the next prompt; background jobs
  alone do not. A shell without marks remains busy while it runs.
- android/setup supplies the login environment and shell hooks; android/vtcore interprets the
  marks and times commands. Changes to prompt parsing or timing read that map as well as the
  session callback contract here.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` creates and holds sessions, reads their state, saves/restores them, applies
  cursor settings, answers clipboard reads, consumes `Events` and releases the sessions.
- `MainActivity` reads `busy`, `cwd`, `progress` and `status` and calls `clear`/`reset`.
- `VtView` attaches through `view`, starts/resizes the session, uses its `tab` handle for
  android/vtcore operations, and sends encoded input through `write`.
- android/vtcore's pump calls the session's `on*` callbacks on the pump thread.

### Outbound

- android/vtcore: `VtNative` terminal handles, creation/restoration, pump, title and at-prompt
  queries, cursor defaults, resize, messages, transcript/snapshot, clear/reset and destruction.
- android/service through main-thread `Events` and `Tabs.MAX_TRANSCRIPT`; android/vtview through
  `view` invalidation and `onStarted`.
- android/native's `Native` (`openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`,
  `consoleCwd`), hence kernel/tty; android/setup's `login.sh` and shell environment.

## Invariants and gotchas

- `ConsolePty.ENVIRONMENT` sets `BROWSER=/usr/local/bin/xdg-open`; browser requests use the
  service's independent endpoint, with no VT escape sequence or per-tab capability.

- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn` and `closeConsole` run on the main thread and take the kernel's BKL, as
  does a resize (`VtNative.resize`, which resizes the kernel console from `vt.rs`; `Native.resize`
  is no longer used by the app); input is written on each tab's own thread, so a program that stops
  reading blocks only its tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- A `VtSession` closes its console exactly once (`hangUp`), so a later close cannot hang up a tab
  that reused the number.
- Snapshots are recorded with the libghostty-vt version that wrote them (`1.0.0`; the copy in
  Ghostty 1.4.0 recorded `ghostty-1.4.0/0.1.0-dev`). One from another version is not decoded,
  since the snapshot format is versioned apart from the library and outside its ABI promise, and
  an app update is the commonest way a run ends with tabs to restore; those tabs come back from
  their transcripts.
- A progress or status report from a command that exits at once (a `printf` at the prompt) is
  usually dropped as soon as it shows: the read after it finds the shell back in the foreground. A
  report made after the reporting group has already given the terminal back is kept until the next
  command takes the foreground. A program in the background reporting is tracked against whatever
  group was in the foreground then.
- The status records live on the main thread only; the pump hands reports over through a locked
  list drained by one post, so the reports of one read are usually applied together and a passing
  state between two of them notifies no one. A program's full reset (RIS) arrives as a clear of
  every record and a removal of the progress, before libghostty-vt's reset callback (which only
  forgets how the prompt moves its cursor); the app's Reset terminal sends neither, nor calls the
  callback, so vt.c forgets that itself there (android/vtcore's Clearing and resetting).
- Shell integration lives in the guest (`/etc/profile.d/tuxport.sh` and the files it has the shells
  read): a tab's behaviour (closing
  without asking, jumps, Select output) follows whatever the shell in it sends, so a user's own
  prompt or shell changes it, and a remote shell over ssh can send marks too.
- Native operations must go through android/vtcore's locking contract. Release sessions and
  close consoles before shutting down the kernel so the pump can return; its per-call kernel
  lookup must not be replaced with a session-long `Arc<Machine>`.
- The clipboard question is asynchronous: answer through the session's writer, never from a
  native callback holding the terminal lock. The reason and unsupported protocols are in
  android/vtcore.

## Confirmed bugs / technical debt

- Progress and status records still follow the foreground process group rather than the next
  prompt, although android/vtcore installs the OSC 133 callback for command timing. A report
  from an already-exited or background program can therefore outlive its intended command.
- Shell mark coverage and native parsing limitations remain in android/vtcore's Shell integration
  debt; `busy` and command-completion events inherit those limitations.

## Open questions

- Should a status notification name the record's `app` when it has no title, as a key for grouping
  (the specification says it is not a label), rather than the tab's title?
- Should a background tab's progress and status be dropped at the next prompt mark rather than
  when the foreground group changes?
- Should the menu's Reset terminal also drop the tab's progress and status records, as a program's
  RIS does?

## To verify

- Session behavior was tried only on a Pixel 4a API 34 emulator (4 KB pages), not real or
  16 KB-page devices: progress/status from real programs (cargo, systemd, AI coding CLIs) and
  the copy toast on Android 12 and earlier remain unverified.
- End-to-end restore with images, bash's marks, a program on the alternate screen at saving,
  or across a library version change remains unverified; the native checks and shell coverage
  residuals are in android/vtcore.
