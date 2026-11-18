# Terminal sessions (android/vt) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vt"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on the tabs' sessions made before android/vt was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49.

## Active

### D1: android/vt owns Kotlin sessions and console lifecycle
- **What**: android/vt owns VtSession.kt and ConsolePty.kt, session events/state, asynchronous clipboard answers, restore fallback and session-side terminal calls, with native state and mechanisms in android/vtcore.
- **Why**: The session map mixed Kotlin lifecycle with native protocol, parsing and locking details.
- **Tradeoffs**: Session/native changes require both maps, while frame encoding and rendering stay in android/vtrender and interaction in android/vtview.
- **Watch out**: Preserve callback threading, exactly-once console closing and snapshot-result contracts across the boundary.
- **Source**: d42013d; 9015b244, 81f60bef; d053be24; 39e55b4b

### D2: A tab keeps only the part of its transcript a save keeps
- **What**: `transcriptText` takes the last `Tabs.MAX_TRANSCRIPT` characters, keeps them and returns them, so a kept and a fresh transcript of the same screen are the same string, and `Tabs` makes the constant `internal` for it while still cutting what it saves.
- **Why**: The transcript kept for saves while the screen is unchanged or on the alternate screen held the whole scrollback's text, of which saving keeps only the last 256 K characters.
- **Tradeoffs**: The formatter and JNI still make the whole text once per save before it is cut, and `VtSession` now depends on a constant of android/service's `Tabs`.
- **Watch out**: A cut can start inside a surrogate pair or a line, as `Tabs`'s own cut already could.
- **Source**: ca5f5c2

### D3: Background tabs' Kitty images are cut by lowering the storage limit
- **What**: `tuxport_vt_drop_images` sets `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT` to `KITTY_TRIMMED` (4 MiB) and back to `KITTY_STORAGE` (32 MiB) on both screens, called by `VtNative.dropImages(tab)` under the tab's lock (`Tab::term`) when the service trims under memory pressure (android/service D4), leaving the transcript and the snapshot state alone.
- **Why**: A tab's Kitty images (up to 32 MiB a screen) could not be given back, and libghostty-vt's C API can neither compress, drop nor count them; only lowering its storage limit evicts.
- **Tradeoffs**: Images are deleted outright with their placements, shown ones included, a program placing an image again by id finds it gone, and a restore from the snapshot saved before the cut brings the deleted images back.
- **Watch out**: A libghostty-vt upgrade could change how lowering the limit evicts or what a limit change marks as mutated, and the view's frames rely on the placements generation changing.
- **Source**: c1a37da

### D4: VtNative is the shared cursor-style namespace

- **What**: VtNative defines all five cursor styles, including frame-only hollow and lock values, and the view/frame/renderer code references them.
- **Why**: Repeated numeric cursor definitions could drift.
- **Tradeoffs**: Two frame-only values are not accepted by setCursor.
- **Watch out**: Keep the numbers aligned with GhosttyCursorStyle and vt.c’s lock value.
- **Source**: aaa1e7d5; 20137e4c; d053be24; 39e55b4b

### D5: Fish and nushell use their own prompt marks and inherited environment

- **What**: No extra fish/nushell startup file is installed because their shells supply marks and ConsolePty’s TERM, LANG and NMAP_UNPRIVILEGED survive login.sh’s su.
- **Why**: The maps incorrectly grouped fish with unmarked shells and implied it needed environment injection.
- **Tradeoffs**: Shells bypassing the profile show no motd, and nushell declares no tap-cursor behavior.
- **Watch out**: Marks were observed in containers and environment propagation was verified by code, not in the app.
- **Source**: a7d8c9f5; 20137e4c; d053be24; 39e55b4b

### D6: Ash continuation parsing excludes background writer runs

- **What**: With tuxport_lf=output, B-marked ash input treats its CR/CR/LF row breaks as continuations and other foreground line feeds as output starts; writer-tagged background runs are bracketed with output and continuation/input marks without ending input or starting command timing.
- **Why**: Long command rows and background job output otherwise incorrectly changed prompt state and busy/timing behavior.
- **Tradeoffs**: Parsing depends on busybox row breaks and ONLCR, and taps above the resumed row after background output still count incorrectly.
- **Watch out**: OSC/DCS strings are tracked but split CSI sequences remain a risk, old edited profiles keep I marks, and no-job-control writers cannot be distinguished by process group.
- **Source**: a809c5a8, daab4446; 20137e4c; d053be24; 39e55b4b

### D7: Bash hooks move to a companion script without changing terminal parsing

- **What**: The profile and Alpine’s system bashrc source tuxport.bashrc, recognized profile hashes migrate existing unedited installations, and terminal parsing retains ash support.
- **Why**: New Alpine bash defaults need prompt recognition and nested interactive-shell timing.
- **Tradeoffs**: Nested bash depends on Alpine’s hook and remains unmarked on Debian.
- **Watch out**: PROMPT_COMMAND replacement or edited profiles can prevent integration; broader prompt and redraw checks remain outstanding.
- **Source**: 7aa817af; 20137e4c; d053be24; 39e55b4b

### D8: Sessions expose a stable BROWSER launcher without transport state

- **What**: ConsolePty sets BROWSER=/usr/local/bin/xdg-open while browser transport remains outside the terminal parser.
- **Why**: CLI discovery needs a conventional launcher independent of a tab's lifetime.
- **Tradeoffs**: Programs may ignore or override BROWSER.
- **Watch out**: A preserved custom launcher can behave differently from the Tuxport client.
- **Source**: 8e211142; d053be24; 39e55b4b

## Superseded
- ~~Background output at an ash prompt ending input like Enter (a809c5a8)~~ → replaced by **D6** in daab4446 (2026-11-13)
- ~~D1 ownership of VtNative.kt, vt.rs and native terminal sections of vt.c~~ → replaced by **D1** in 81f60bef (2026-11-14)
