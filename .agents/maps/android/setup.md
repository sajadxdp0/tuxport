# android/setup Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/setup.md` for the paired decision history.
>
> Verified: 2026-11-15 (full mapping: 4 research concerns; 12 claims checked by an independent verifier — 8 confirmed and 4 qualified below)
>
> Maintained: 2026-11-18 (targeted verification only since the full mapping; latest change: localized privacy-policy entry in Settings About using the shared web-link action)

## Responsibilities

`android/setup` provides the guest system and its account setup:

- the distributions (Alpine from the APK, Debian downloaded), downloading and verifying tarballs,
  importing them into a new store, and writing first-boot configuration, which on Alpine makes
  bash, bundled with the app, the login shell;
- files refreshed before every kernel start (network interfaces, rootless podman's network, the
  bundled doas package, the profile script and the files of ash's, bash's and zsh's prompt marks for the
  terminal's shell integration), the osclock script run before boot and the boot log's failed services;
- package mirrors: built-in lists, one speed probe after an import, writing the fastest into
  sources nobody has changed;
- resolving the login account and the per-tab login script (`login.sh`), including the first-login
  admin setup, installing the bundled bash, procps-ng and bubblewrap, and the NOPASSWD rules;
- the settings screen, which displays service state and forwards choices, plus shared privacy
  and notification authorization actions and navigation to system battery settings;
- the store lock shared by the kernel start, store deletion and the Files provider.

Root: `android/app/src/main/java/io/github/sajadxdp0/tuxport/Rootfs.kt`, `SettingsActivity.kt`,
`android/app/src/main/assets/login.sh`, `android/app/src/main/assets/profile.sh`,
`android/app/src/main/assets/ashrc.sh`, `android/app/src/main/assets/bashrc.sh`,
`android/app/src/main/assets/zshrc.sh`, `android/app/src/main/assets/xdg-open.sh`, `scripts/fetch-rootfs`.

Outside the module: when imports, switches, restarts and logins happen, and the `settings`
preferences (`android/service`); the import and `write_file`/`read_file` implementations
(`kernel/store`, `kernel/vfs`); bundling the assets (`android/build`); the shared page layouts
(`android/terminal`), the licences (`android/licenses`) and the authentication gate (`android/privacy`);
what the terminal does with the prompt marks (`android/vtcore`, `android/vtview`) and the Kotlin
session consuming their state and events (`android/vt`).

## Key types

- `Rootfs` — object: `Distro` (`ALPINE`, `DEBIAN`: label, download, mirrors, probe path),
  `Download` (urls, sha256, size), `Cancelled` and the private `Oversize` (both `IOException`s),
  `Account` (name, uid, gid, home from `/etc/passwd`), `access` (a `ReentrantReadWriteLock`).
  - `ALPINE`: labelled with the branch the APK bundles (`BuildConfig.ALPINE_BRANCH`); no download;
    mirrors are URLs ending in `/alpine` (dl-cdn, Tsinghua, USTC, Aliyun), probed at the branch's
    `main/aarch64/APKINDEX.tar.gz`.
  - `DEBIAN`: "Debian 13"; one pinned GitHub URL (debuerreotype's `debian:trixie`) with fixed
    length and sha256; mirrors are host names (deb.debian.org, Tsinghua, USTC, Aliyun), probed at
    `debian/dists/trixie/InRelease` over https.
  - The first mirror of each list is the official one: the import writes it, and probing falls
    back to it.
- Preferences: file `rootfs`, owned here (`distro`); the `settings`
  file is android/service's `Settings` (`tier`, default 1 in all builds; `login`, `run_policy`, `idle_policy`,
  `file_access`, `fill_wide`, `cursor_style`, `cursor_blink`, `volume_keys_switch_tabs`, `show_tab_list`),
  which Settings shows through the service.
- `SettingsActivity` — an `AppCompatActivity` hosting `SettingsFragment` (in the same file), a
  `PreferenceFragmentCompat` and `TerminalService.Listener`, on the Material 3 frame it shares
  with android/licenses (`android/terminal`'s `page.xml`). `PreferenceCategory` groups standard
  `Preference` actions, `SwitchPreferenceCompat` toggles (a `MaterialSwitch` widget) and
  `ListPreference` choices (login account, screen-off running and cursor shape). Groups: System (distro, login account,
  reset), Android (conditional notification and battery usage actions, running policy, privacy mode, file access), Terminal (CJK characters filling two
  cells, the cursor's shape, whether it blinks, tab-list button visibility, the volume keys switching tabs), Debugging (execution
  mode and diagnostic export, in all builds) and About (Telegram, project website, privacy policy, licenses, version).
- `login.sh` — `sh -c <script> tuxport-login <login> <cwd>`, run as root on each tab's console, or
  without a console with `--refresh-rules`.
- `profile.sh` — copied verbatim, comments included, into the guest as
  `/etc/profile.d/tuxport.sh`; `Rootfs` keeps the texts of the versions without prompt marks
  (`OLD_PROFILE`, `UNMARKED_PROFILE`) and the sha256s of the later ones (`REPLACED_PROFILES`) to
  recognise them.
- `ashrc.sh` — copied verbatim into the guest as `/etc/profile.d/tuxport.ashrc` (not `.sh`, so
  `/etc/profile` does not read it): ash's prompt marks, read through `ENV`.
- `bashrc.sh` — copied verbatim into the guest as `/etc/profile.d/tuxport.bashrc`: bash's prompt
  marks, read by the profile and, on Alpine, by `/etc/bash/tuxport.sh` (`BASH_HOOK`, a line sourcing
  it).
- `zshrc.sh` — copied verbatim into the guest as `/etc/profile.d/tuxport.zshrc`, read by the
  profile only in zsh. Its anonymous function registers the hooks in local zsh emulation,
  preserving the caller's options. `Rootfs.writeProfile(context, store, distro)` writes all three
  companion files and then the profile.
- Guest-side state: `/var/lib/tuxport/admins` (recorded admins), `/var/lib/tuxport/doas-installed`
  and `sudo-installed` (markers), `/etc/doas.d/20-wheel.conf`, the NOPASSWD rules
  (`/etc/doas.d/30-tuxport.conf` or `/etc/sudoers.d/tuxport`), `/var/cache/tuxport/doas.apk`,
  `/var/cache/tuxport/{bash,procps,bubblewrap}/` (packages until installed), and `/run/tuxport-install.lock`.

## Public entry points

- `Rootfs.prepare(distro)` (with an optional store, used by the instrumentation tests), `download`,
  `fetch`, `isReady`, `installed`, `importing`, `store`, `reset`, `access`.
- Before every start: `ensureNetworkConfig`, `ensureContainersConfig`, `ensureLoginTools`, `ensureBrowser`,
  `updateProfile`; around boot: `PREFER_OSCLOCK`, `failedServices`.
- Mirrors: `chooseMirror`, `useMirror`.
- Accounts: `loginAccount`, `regularAccounts`, `dirsChanged` (with `REFRESH_RULES`).
- `scripts/fetch-rootfs [outdir]` (build-time download of the Alpine minirootfs and the bundled
  packages into `target/rootfs` by default; prints each package's path, then the tarball's last).
- `login.sh` arguments: login setting (`auto`, `root` or an account name) and an optional start
  directory; or `--refresh-rules`.
- `SettingsActivity`, opened from the terminal's ⋮ menu (not exported).

## Data flow / lifecycle

### Tarballs

- Alpine: `scripts/fetch-rootfs` downloads the bundled packages from the branch's `main`
  repository (doas, and bash with readline, libncursesw and ncurses-terminfo-base, each checked by
  its pinned sha256 only; also procps-ng, libproc2, libintl, utmps-libs and skalibs-libs, sharing
  the ncurses dependencies; also bubblewrap and libcap2; apk checks signatures at install) and, unless a copy with the
  pinned sha256 is already there, the minirootfs: its GPG signature is
  checked when both gpg and gpgv are installed (a failed signature download counts as bad; without
  gpg a warning), then its pinned sha256. Gradle bundles the newest fetched files by version
  (`BuildConfig.ALPINE_BRANCH` is taken from the name, `edge` without one) as `rootfs.tgz`,
  `doas.apk`, `{bash,procps,bubblewrap}/<file name>` without checking them again, and fails without them; `prepare`
  copies the tarball asset to the cache.
- Debian (`download`): a cached `cacheDir/<distro>.tar.gz` with the pinned length and sha256 is
  reused; else `fetch` tries the sources in order (probed by speed when there are several; Debian
  has one), checking for cancellation before each, moving to the next on any error but a cancel,
  and throwing the last error.
- One source: 30 s timeouts; written to `.part` while hashing (64 KiB reads, cancellation checked
  before each, progress every 0.5%); an attempt that failed after getting data is resumed with
  `Range` up to 5 times, and one that got none ends the source. A resume answered 206 continues,
  one answered 200 starts again from zero, and any other status ends the source like an attempt
  that got no data. An attempt that broke off with the whole pinned length received is not resumed
  but verified at once (a `Range` from the end would be answered 416). More than the pinned length
  stops the source at once (no resume). Length and sha256
  are checked and `.part` is renamed into place; any other exit deletes `.part` (a kill can leave
  it), and every call starts from zero, truncating it, so nothing resumes across calls or kills. A
  206 reply's range is not checked; the final hash catches a wrong one.
- A switch's download can be cancelled (between reads; a blocked connect or read waits for its
  timeout); a download inside `prepare` cannot.

### Import

- `prepare` (does nothing for a ready store, one whose `meta.db` exists):
  1. for the app's store, commit the distro it imports (`distro`), the only record;
  2. delete `<store>.tmp`;
  3. get the tarball into the cache: Alpine's asset copied over it, unchecked; others through
     `download`;
  4. import into `<store>.tmp` (`Native.importRootfs`) and `configure` it;
  5. sync the store (`syncfs`), rename it to the real store, fsync `files/` (a failure only
     logged) and only then delete the tarball.
- `installed` is the recorded distro of a ready store (Alpine when there is no record); while
  there is no store, `importing` names the distro an import was last started for (Alpine when
  there is none), which the service imports at the next launch. `reset` deletes `rootfs.tmp` and
  the store under the write lock and leaves the preferences and the cache alone.
- `configure` writes, with mode 0644 for new files (an overwritten file keeps its mode, and an
  existing entry that is not a regular file fails the import):
  - always: package sources from the first mirror (apk `main` and `community`; Debian deb822 over
    http for trixie, -updates, -security), `hostname` (`localhost`), `motd`, and
    `/etc/profile.d/tuxport.ashrc`, `/etc/profile.d/tuxport.bashrc`, `/etc/profile.d/tuxport.zshrc`, on Alpine
    `/etc/bash/tuxport.sh`, and then `/etc/profile.d/tuxport.sh` (the assets' `ashrc.sh`,
    `bashrc.sh`, `zshrc.sh` and `profile.sh`: motd, `TERM`, `LANG`, `NMAP_UNPRIVILEGED`, the browser default below,
    `ENV` naming the ash file unless one is set or the file is unreadable, and for bash, busybox ash and zsh only a
    coloured prompt with prompt marks, below; other shells keep the distro's prompt);
  - Alpine: `hosts` only when absent (the minirootfs ships one, so nothing is written), and no
    `resolv.conf` (the kernel writes one pointing at the DNS stub at every start when it is
    missing); bash as the login shell (`useBash`): the bundled packages from the assets' `bash/`
    into `/var/cache/tuxport/bash/`, and, when there were any, root's `/etc/passwd` shell changed
    from `/bin/sh` to `/bin/bash` (any other shell is kept). Systems imported before keep busybox
    ash; accounts busybox's `adduser` makes later take the shell of `$SHELL` or of the account
    running it, so from a root tab they get bash too; procps-ng and its six dependencies are
    staged in `/var/cache/tuxport/procps/` by `cachePackages`, also used for bash and bubblewrap
    with libcap2 (`/var/cache/tuxport/bubblewrap/`). Existing stores receive neither new bundle;
  - Debian: `hosts` and `resolv.conf` (the DNS stub) always.

### Before every start and boot

- `ensureBrowser` installs `xdg-open.sh` as `/usr/local/bin/xdg-open` (0755) on either distro.
  It preserves custom launchers, replacing only the known previous Tuxport OSC helper by SHA-256.
  Before every kernel start it refreshes the static `/usr/local/libexec/tuxport-open-url` (0755)
  and `/etc/tuxport/browser` (0644: current loopback port and capability, shared by guest users).
  The shim execs that client; `ConsolePty` sets `BROWSER` to the shim (android/vt).
  The login profile also exports that default when `BROWSER` is unset or empty and the shim is
  executable, restoring browser discovery after `su -` clears the inherited environment.
  Nonempty custom values are preserved. This requires a shell that reads `/etc/profile`;
  shells such as fish and Debian's zsh need their own login configuration.
  Each invocation rereads the configuration and sends one HTTP(S) URL without using a TTY or
  inherited tab token; success acknowledges Android intent acceptance, not page loading or login.
- On the start thread, after an import and the start of its mirror probe when there is one, in
  this order, before the kernel starts under the write lock: a loopback-only `/etc/network/interfaces` when absent;
  `/etc/containers/containers.rootless.conf.d/tuxport.conf` setting `netns = "host"` when absent
  (rootless podman's pasta and slirp4netns need a TUN device Tuxport lacks; only rootless podman
  reads that directory, and a user's own `containers.conf` overrides it); on Alpine the bundled
  `doas.apk` into `/var/cache/tuxport/`, rewritten at every start; a profile left exactly as an
  earlier version wrote it (with `cd "$HOME"`, without prompt marks, or with marks that do not
  declare `cl=line`, or with ash's, bash's or zsh's marks in itself) replaced by the current one, with
  the mark files written before it (`/etc/bash/tuxport.sh` on Alpine stores, bash installed or
  not); an edited one is kept and a missing one not recreated, and then the mark files are not
  written either. The first two apply to every distro. A failed write in any of them fails the start.
- Storage is left to the packages: podman's overlay driver and docker's (containerd's) overlayfs
  snapshotter run on the kernel's overlayfs, rootful and rootless. A `storage.conf` choosing vfs or
  a `daemon.json` choosing native that an earlier build wrote stays until the user removes it.
- On the boot thread, which runs while tabs can already log in (a restored tab starts its shell
  when first shown): `PREFER_OSCLOCK`, without a console, adds osclock to the boot runlevel when
  `rc-update` and the osclock service exist and no runlevel has hwclock, osclock or swclock (a
  dangling link counts); the thread waits for it, ignores its exit status and only logs an
  exception. After boot, `failedServices` lists the services `/var/log/boot.log` says failed to
  start, for the service's notification.

### Prompt marks

- Integrated bash, ash and zsh set OSC 2 to the full current `$PWD` at each primary prompt, with
  control characters replaced by `?`. Before execution, bash and zsh set OSC 2 to the submitted
  command line, also sanitized, once for the whole pipeline or compound command. This is input
  text, not tracking of the foreground executable or its children. Programs can replace it with
  OSC 0/2 between prompts; the next primary prompt restores the directory.
  Bash sends the directory from `__tuxport_prompt`, zsh from precmd,
  and ash after the visible PS1 and B mark. Secondary prompts do not replace the title.
  Bash appends `$(__tuxport_preexec)` to PS0 once and snapshots `HISTCMD` at each prompt. The
  preexec function reads `history 1` with no timestamp only when history is enabled and its
  counter advanced; otherwise it reports `bash`, including ignored duplicate/space-prefixed
  commands, HISTIGNORE matches and disabled history. It does not install a DEBUG trap or
  change history settings. Zsh uses preexec's command argument, independent of history storage.
  The tab bar still shows numbers; these titles are available through long presses and TalkBack.
  Existing shells retain their loaded hooks until a new shell starts or the full profile is
  reloaded. Sourcing ashrc alone into an already-marked old PS1 skips its existing-mark guard.

- For the terminal's shell integration (android/vtcore) the profile has login shells mark their
  prompts with OSC 133:
  - bash: the profile sets the coloured PS1 and reads `tuxport.bashrc`, whose `__tuxport_prompt`
    function, put first in `PROMPT_COMMAND` (once, however often the file is read), prints, before
    each prompt, the last command's end with its status (D) and the prompt's start on a fresh line
    (A, `redraw=last;cl=line`), and wraps PS1 (P with `k=i` … B, the input starting) and PS2
    (`k=s`), and appends the output's start (C) to PS0, each again whenever it is missing, so a PS1
    that `~/.bashrc` or anything later sets is wrapped from the next prompt on; a later plain
    assignment to `PROMPT_COMMAND` drops the function and with it every mark. On Alpine,
    `/etc/bash/bashrc` reads `/etc/bash/tuxport.sh`, hence `tuxport.bashrc`, in every interactive
    bash (a login one first, through `/etc/profile.d/00-bashrc.sh`), so a nested bash (`bash`, `su`
    without `-l`) marks its own prompt, Alpine's `\h:\w\$ `;
  - busybox ash, which has no hook before a command or a prompt (1.37 has no `PROMPT_COMMAND`):
    PS1 itself carries D with `$?` (expanded at each prompt and redraw), A with `cl=line`, and
    after the prompt B (the input starts); PS2 is P with `k=s` and B. ash marks no output start
    and breaks a command longer than the screen's width into rows itself, so A and P also carry
    `tuxport_lf=output`, the app's own option, asking the terminal to read ash's line feeds
    (android/vtcore): one ash breaks a line with continues the input, any other ends it. `tuxport.ashrc`
    wraps whatever PS1 and PS2 are when they have no marks, and only in busybox ash: the profile
    sources it after setting its coloured PS1, and every interactive ash reads it again through
    `ENV`, a login shell after `/etc/profile` and `~/.profile`, so a PS1 that `~/.profile` sets is
    marked; one set later (typed, or by a file the user's own `ENV` names) goes without.
  - zsh: the profile sets the coloured PS1 and reads `tuxport.zshrc`. Its `__tuxport_precmd`,
    appended to `precmd_functions`, prints D with the status before each
    prompt, wraps PS1 (A with `cl=line` … B) and PS2 (P with `k=s` … B) in `%{ %}` whenever the
    marks are missing, and moves itself last in `precmd_functions`, so a PS1 that `~/.zshrc` or a
    theme's own hook sets is wrapped (a theme setting PS1 before each prompt gets its first prompt
    unmarked); `__tuxport_preexec`, appended to `preexec_functions`, prints the output's start (C)
    and the command title. Reloading the profile or companion file deduplicates both hook
    registrations while preserving other hooks, including when sourced in sh emulation.
    A and B are in PS1, so every redraw by zle sends them again; D is sent once per prompt.
  - `cl=line` tells the terminal that the three line editors move the cursor with the left and
    right arrows, so a tap on the typed command moves the cursor there.
- Every login shell of a tab reads it (root's shell `-l`, and `su <account> -c … exec "$SHELL" -l`
  for others) when the shell reads `/etc/profile`: zsh does only where its `/etc/zsh/zprofile`
  sources it (Alpine's does, in sh emulation; Debian trixie's does not, so zsh there has no marks
  unless the user's `~/.zprofile` sources `/etc/profile.d/tuxport.sh`). `ENV` is exported from every
  shell that reads the profile, so a nested interactive ash that inherits it (`ash`, `sh` on
  Alpine, `su` without `-l`, also under bash or zsh) marks its own prompt, the distro's; one
  started through `doas -s`, which resets the environment, does not. Other nested non-login shells
  (but bash on Alpine, above; a nested bash on Debian has none) and
  other shells get no marks from it (fish and nushell send their own, android/vtcore), and shells that
  do not read it still have `TERM`, `LANG` and `NMAP_UNPRIVILEGED` from the console's fixed login
  environment (android/vt), but show no motd.
- The shared profile contains no zsh array syntax; only zsh sources `tuxport.zshrc`. The companion
  uses an anonymous function with `emulate -L zsh` for registration, so Alpine's sh emulation
  does not change array filtering or indexing and the caller's options are restored afterwards.
  Both hooks also use local zsh emulation when invoked.

### Mirrors

- Each distro has built-in mirrors (official, Tsinghua, USTC, Aliyun). After every import, and
  only then, on the `tuxport-mirrors` thread, `chooseMirror` probes every mirror at once (one
  thread each, checking 512 KiB and 5 s thresholds between reads, waiting at most 10 s for each result) and takes the
  highest throughput (ties to the earlier mirror); if every probe failed it does nothing. A read
  can overshoot either probe threshold; these are not hard byte or elapsed-time caps.
- Writing (`useMirror`): only while the distro's sources file (`/etc/apk/repositories` or
  `debian.sources`) is byte for byte what the import wrote, with the official mirror, it is
  written again from the same template with the winner (nothing is written when the official
  mirror won). A file that differs in any way (already switched, edited by the user) or is
  missing is left alone, so the mirror is switched at most once per import and the sources are
  the user's from then on. It does not take the store lock; a failed write is only logged.
- Nothing records the mirror in use and nothing in the app shows or changes it later: the user
  edits the sources in the guest.

### Accounts and login

- `loginAccount`: `auto` is the regular account (UID 1000–65533, a shell other than empty,
  `nologin` or `false`) with the lowest UID, else root; `root`; or a named passwd entry, falling
  back to root. `regularAccounts` lists the same accounts for Settings. `login.sh` picks the
  account by the same rules, except that it does not require a numeric GID or all seven fields.
- `login.sh` traps SIGINT, so Ctrl-C does not end the login but still interrupts the command
  running (normally apk or apt, skipping the install until the next login), chooses wheel, doas and
  `/etc/doas.d/30-tuxport.conf` on Alpine (`/etc/alpine-release`), else sudo, sudo and
  `/etc/sudoers.d/tuxport`, picks the account, and:
  - every login, root's included: rewrite the NOPASSWD rules for recorded admins still in the
    group that have no password: a shadow field that, after any leading `!`s, is empty or `*`
    (`adduser -D` writes `!`); `passwd -l`'s `!` before a hash still counts as a password, and so
    do a missing shadow entry and an unreadable `/etc/shadow`. The file is written beside it,
    made 0440 and renamed into place; with nobody qualifying, or without the rules' directory
    (on Debian, until sudo is installed), it is deleted;
  - `--refresh-rules` as the first argument only rewrites the rules: `Rootfs.dirsChanged` runs
    it as root, without a console and without waiting, when the service's poll finds `/etc`
    itself among the directories the guest changed (`passwd`, `chpasswd` and `adduser` replace
    `/etc/shadow`; group changes replace `/etc/group`), so a password set in a running session
    makes escalating ask for it within about one poll (2 s);
  - before choosing the account: install procps-ng, then bubblewrap offline when their cache directories
    exist, under the install lock, testing `apk info -e <package>` (BusyBox already provides ps/top/free),
    re-add each by name for upgrades, and delete its cache under the lock only on success. Failure
    leaves the cache for the next login and still opens a shell; later user removal is respected.
    This works with any login shell; `--refresh-rules` returns before installing;
  - every login whose account's passwd shell is `/bin/bash` while bash is missing and
    `/var/cache/tuxport/bash` exists: install those packages offline, then add `bash` once more by
    name (so world records it and `apk upgrade` upgrades it), under the install lock (below), and
    delete the directory once bash is there, so bash removed later is not installed again; if it
    is still missing (Ctrl-C while the install waited, below), say so and use `/bin/sh` this time,
    and the next login tries again;
  - root: `cd` to the directory (else root's home, else `/`) and `exec` root's passwd shell (or
    `/bin/sh` when it is not executable) as a login shell;
  - a regular account not yet in `/var/lib/tuxport/admins` (matched literally): add it to wheel
    (Alpine, with `permit persist :wheel` in `20-wheel.conf`) or sudo (Debian) and record it once
    it really is in the group (otherwise say so and retry at the next login);
  - a regular account, while the tool's marker is missing and the tool is not on PATH (whether or
    not the account was just made an admin): install doas from the bundled package and re-add it
    by name under the install lock, or sudo from the network showing apt's progress (Ctrl-C skips
    until the next login; no install lock is taken: `apt_update` retries `apt-get update` every 2 s
    while another apt holds the package lists' lock, saying so once and showing apt's other errors
    and warnings after it ends, and `apt-get install` waits up to an hour for dpkg's lock through
    `DPkg::Lock::Timeout`, which apt shows on the terminal), and touch the marker once the tool is
    there;
  - the install lock: a `flock` on `/run/tuxport-install.lock` held around each install of a bundled
    package (`locked`), so concurrent logins install bash, doas, procps-ng and bubblewrap one after another, each first
    checking whether an earlier holder installed it already; each `apk add` (`apk_add`) that fails
    on apk's own lock, held by an apk the user runs, prints that another apk is running and is
    retried with `--wait 3600`, so the login waits until that apk ends; Ctrl-C stops the wait and
    skips the install until the next login;
  - switch with `su <account> -c '<cd dir or home>; exec "$SHELL" -l'` (`--session-command` when
    util-linux su offers it), adding `-s /bin/sh` when the account's shell is not executable (su
    would otherwise fail and the tab close); package commands read from `/dev/null` so early typing
    reaches the shell.

### Settings

- The Android group starts with an Enable notifications action only while app notifications are
  disabled, regardless of whether the terminal banner was dismissed. It uses android/terminal's
  `NotificationPermission` helper and shared request history: request once on Android 13+, then
  open system notification settings. The action checks privacy and works without service binding.
  The fragment refreshes visibility when creating preferences, after permission results and on
  resume, including return from system settings. Granting removes it; revoking restores it.
- App battery usage follows notifications and precedes the screen-off policy. It opens the
  system app details for this package. The row reads the system Doze exemption on creation and
  resume, remains available after banner dismissal, and disappears when the package is exempt.

- Diagnostic export is independent of service binding and kernel readiness. Its protected dialog
  offers Save to file or Share. Both call `Privacy.requestLogExport` before generating a ZIP;
  privacy mode requires fresh authentication even when Settings is already unlocked.
  Only the authorized callback starts `tuxport-export`. A stopped/replaced fragment or expired
  authorization deletes its prepared result instead of handing it off.
- Save stages its already-prepared ZIP in a fragment-scoped `PendingLogSave` ViewModel and then
  launches `CreateDocument("application/zip")`. The mode and pending file reference survive
  configuration recreation, never process death; the authority only covers this archive.
  A picker result consumes the reference once. Cancellation or permanent owner destruction while
  pending deletes the file; a valid destination transfers ownership to `tuxport-save-logs`, which
  opens the source before `openOutputStream("wt")` and deletes the ZIP after success or failure.
  Save files use `cache/diagnostic-saves`, isolated from share-cache pruning; startup attempts to
  remove orphans from a dead process. The row stays disabled while preparing, pending or saving.
- A consumed picker result calls `Privacy.deferNextUnlock` while the page is paused. Its next
  gate resume can show the locked shield without a redundant automatic authentication prompt;
  this never unlocks Settings or permits new exports. The already-authorized transfer may finish
  while locked/backgrounded. Ordinary unlock cannot replay a cancelled or consumed save.
  Share instead grants temporary read-only access through `Diagnostics` after its final check.
  Failures restore the row and show a localized message. A provider failure may leave an empty or
  incomplete destination; cancelling authentication occurs before the picker creates any document.
- Privacy mode is an authenticated exception to service-owned changes: its nonpersistent switch asks before enabling, then calls the shared `Privacy` gate; disabling also authenticates. No setting changes on cancellation or failed/unavailable authentication. File access stays disabled while privacy is on, with a localized explanation; turning privacy off leaves file access off. Material dialogs use `showPrivate`, and restored choice dialogs use `Privacy.protect`.
- Binding: the fragment's `onStart` starts and binds the service, `onStop` unbinds; once bound it listens,
  renders, shows a pending notice and loads the regular accounts on a background thread, again
  when the kernel is ready; account loads use the application context and a generation to discard
  results after stopping or a newer load. `onSettingsChanged` renders; `onProgress`, `onRestarting` and
  `onExitAll` finish the screen, so a switch, reset or restart goes back to the terminal, which
  shows the progress.
- Service-owned rows read the service's state and call it:
  - distro: usable during a switch (it then cancels the download, after a confirmation) or when a
    switch is possible, a start failure included; choosing another distro or Reset asks first;
  - login account: whenever a store is installed and no switch runs, with a toast naming the
    account new tabs log in as;
  - reset: whenever a switch is possible;
  - execution mode (tier, in all builds): while the kernel runs with no switch, a different tier
    asking to restart; the summary names the current mode and Tier number, with a slower hint
    for Tier 0. Separate choice labels mark Tier 1 as the default and Tier 0 for troubleshooting;
  - screen-off running (Allow sleep, Run while busy or Always run, with the selected policy's
    description; BUSY explains its 30 s idle grace and network/scheduled-work limitation),
  file access (outside privacy mode), CJK fill, cursor shape (Block, Bar or Underline, the summary
    naming the current one), cursor blink, the volume keys switching tabs and showing the tab-list
    button (all three on by default): usable at any time; the service gives every tab the new cursor values at once
    (android/service), and the terminal reads the CJK, volume-key and tab-list visibility values itself
    (android/terminal).
- About starts with a support link (`https://github.com/sajadxdp0/tuxport/issues`),
  followed by the project website (`https://github.com/sajadxdp0/tuxport`) and the privacy policy
  (`https://sajadxdp0.github.io/tuxport/privacy.html`).
  All three use the shared `link` preference helper, show their URL
  as the summary and open it with AndroidX `CustomTabsIntent`, without pinning a browser package.
  The support label is non-translatable and used in every locale. A browser without Custom
  Tabs can handle the same VIEW intent; no handler produces the shared `link_no_app` toast.
  The licenses summary names GNU AGPL 3.0 or later; the full copyright, warranty disclaimer and
  license text remain in the Tuxport notice on the license page.
- About's final Version action shows `BuildConfig.VERSION_NAME` followed by `BuildConfig.VERSION_CODE`
  in parentheses (`1.0.0 (1)`). It opens the project's GitHub Releases page
  (`https://github.com/sajadxdp0/tuxport/releases`) with a VIEW intent; no URL handler shows
  `link_no_app`. Tuxport is not published on Google Play.
- Service-dependent preferences are disabled before binding, on disconnect and on stopping;
  notification authorization, battery usage, diagnostic export, support, project website, privacy policy, licenses and version remain usable. All preferences have stable resource-name keys and persistence
  disabled: they render service values and their change listeners call service setters, never
  writing a second preference store (privacy, notification authorization and battery usage use shared controllers). AndroidX manages row binding, disabled states and
  accessibility; `SwitchPreferenceCompat` binds the Material widget through `switchWidget`.
- Login, running policy and cursor choices use `ChoiceDialog`, a `ListPreferenceDialogFragmentCompat` whose
  builder is `MaterialAlertDialogBuilder`: AndroidX keeps the entries, selection and dialog
  state, and Cancel, Back or an outside tap commits nothing. Distribution, reset, tier and notices
  keep their explicit Material dialogs and confirmation flows. File access also asks on every enable,
  warning about reading or copying sensitive home/system files; only Enable saves true, while
  Cancel, Back, outside tap or recreation leaves it off. Disabling acts immediately through the service.
- Look: Material 3 light or dark with the system, with the wallpaper's colours from Android 12 on
  (`DynamicColors`), edge to edge. A dark-mode or display size change recreates the screen (it
  handles neither `uiMode` nor `density`), which restores the fragment, scroll position and an
  open login/running-policy/cursor choice dialog, then rebinds and renders. Other open dialogs are dropped.
  The activity applies top and side insets; the fragment pads its RecyclerView above navigation
  bars with clipping disabled, so the final preference can scroll clear of them.
  Adapter state restoration waits for the first service render and window insets, so initially
  empty summaries and missing bottom padding cannot shift the saved scroll position.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (start, import, switch, reset, the mirror probe, per-start files, boot, the poll's
  `dirsChanged`, login accounts), `Documents` (`loginAccount`, `installed`, `isReady`, `access`),
  `ConsolePty` (runs `login.sh`), `MainActivity` (opens Settings), the instrumentation tests
  (`fetch`, `prepare` into another store, `useMirror`, `updateProfile`, `PREFER_OSCLOCK`).

### Outbound

- `Native.importRootfs`, `syncRootfs`, `writeFile` (creates parent directories, does not follow
  symlinks), `readFile` (null when missing), `spawn` and `waitFor`; `HttpURLConnection` (system
  proxy); `BuildConfig` (Alpine branch, app version and application ID); the assets; `TerminalService` and its
  `Listener` (Settings); `LicensesActivity` (android/licenses); android/privacy's mode changes,
  export authorization and dialog protection; Material Components and AndroidX (Settings),
  including AndroidX Browser for Telegram, the project website and privacy policy; android/terminal's NotificationPermission
  for shared authorization history and settings navigation, and BatteryOptimization for exemption status and app details.

## Invariants and gotchas

- Settings must set the app bar's `liftOnScrollTargetViewId` to AndroidX Preference's
  `recycler_view`: the list is nested inside the fragment container. Without that target,
  Material's layout and nested-scroll callbacks inspect different views, so preference updates
  can reset the app bar's lifted background even while the list remains scrolled.
- `Rootfs.access`: provider calls hold the read lock; starting the kernel and deleting the store
  hold the write lock, so the kernel's exclusive boot open never overlaps a provider open. The
  mirror write, the import (on `rootfs.tmp`) and the per-start files run without it.
- The probe starts as the import ends, so it can write the sources while the kernel opens the
  store (a distro switch's `replace` joins the mirror thread before deleting the store). The write
  opens its own store connection: at worst the kernel then skips one boot
  cleanup (the store's leftovers under `/dev/shm` and `/run`, hidden by the tmpfs mounts, and
  orphaned objects reclaimed later), with no corruption.
- A store becomes ready only when the synced import is renamed into place, so a half-imported
  `rootfs.tmp` never counts and is deleted by the next `prepare` or `reset`.
- An import interrupted by a kill (a first run, a distro switch, a reset) is redone with the same
  distro on the next launch, from the verified tarball still in the cache or by downloading it
  again with the usual progress; if that fails, the service falls back to Alpine with its notice.
- The distro record is committed before the import starts, so an existing store always has it;
  only the app's own store writes it, but an import into another store (only the
  instrumentation tests make one) still overwrites (Alpine) and deletes the shared cached tarball.
- `writeFile` while the kernel runs is seen at once (the store's commit counter invalidates kernel
  caches).
- `updateProfile` replaces only the versions it knows, compared whole (`OLD_PROFILE` and
  `UNMARKED_PROFILE` by text, later ones by sha256 in `REPLACED_PROFILES`): changing `profile.sh`
  must add the sha256 of the text it replaces (the committed asset, byte for byte), or stores keep
  the version they have; `ashrc.sh`, `bashrc.sh` and `zshrc.sh` are written only with the profile,
  so a change to any companion alone reaches no existing store (change `profile.sh` with it); and
  the motd is written at import and refreshed on startup by `Rootfs.updateMotd`, which replaces
  only the known old text (the one whose example installed the Codex CLI), leaving an edited one.
- apk takes its database lock without waiting unless given `--wait` (which apk 3 accepts though
  its help does not list it): hence the install lock and the retry with `--wait`; a tab whose login
  waits for an apk the user runs shows only that line until the apk ends or Ctrl-C. apt 3's
  `DPkg::Lock::Timeout` covers dpkg's frontend lock but not the package lists' lock, which
  `apt-get update` takes, hence the retries there; apt shows its own wait only on a terminal. The
  branch's mirrors keep only each package's latest build,
  so a rebuild of a pinned package makes a fresh `scripts/fetch-rootfs` fail until it is pinned
  anew.
- The marks rely on the shell redrawing its prompt on SIGWINCH (the terminal clears the prompt's
  rows on a resize; android/vtcore): bash, busybox ash and zsh do.
- The rules refresh writes in `/etc/doas.d` or `/etc/sudoers.d`, not `/etc` itself, so it does not
  trigger itself; the first login's group change and `mkdir /etc/doas.d` cause one more refresh.
- `login.sh` runs without job control, so its children share its process group: a Ctrl-C that
  lands during another command than an install (the passwd lookup, the group check, the shadow
  read, the `su --help` probe) can log a named account in as root, leave an admin out of the
  NOPASSWD rules until the next refresh, or make Debian fall back to `su -c`. The window is short.

## Confirmed bugs / technical debt

- Mirror template comparison and replacement are separate operations: a guest edit between
  them can be overwritten.
- `locked` runs the install command even when `flock 9` fails. The unchecked return is confirmed;
  consequences under lock errors or interruption have not been reproduced.
- Busybox ash can emit literal path controls through the visible prompt's `\w`. The directory
  title is sanitized and restored after that prompt, but other terminal effects in visible
  prompt text are not sanitized.
- Automatic command titles cover bash and zsh only. Busybox ash has no preexec hook; it still
  needs terminal-side or foreground-process detection to report commands that set no title.

- **The rules miss changes that rename nothing in `/etc`.** A shadow or group file edited in place
  (an append, a text editor writing the file itself) changes no entry of `/etc`, a burst of more
  than 256 changed directories between two polls stops recording the rest, and changes before the
  service's first poll are not recorded, so such a change waits for the next login; while the app
  is not running there is nothing to refresh.

## Open questions

- Should a failed optional per-start write (the network, containers, doas package or profile file,
  for example through a directory the user replaced with a symlink, which `writeFile` refuses) fail
  the whole start rather than be logged?
- Should a Debian tarball left in the cache after its import failed and fell back to Alpine be
  deleted?

## To verify

- The zsh companion extraction passed host zsh checks in native and sh emulation: byte-identical
  prompt/status/title output to the prior profile, repeated loading, existing-hook preservation,
  theme-like hook ordering and caller-option restoration. BusyBox ash and Debian dash parse and
  source the shared profile. Debug assembly and packaged-asset checks passed; the prior
  stock-profile hash is registered for migration. Android device execution and end-to-end migration of this extraction remain unverified.

- Browser defaults passed interactive `su -` checks for root and a regular user with Alpine
  ash/bash and Debian dash/bash in container PTYs, including launcher invocation with an intact
  URL, custom/empty values and a non-executable launcher. Debug assembly passed. The updated
  profile and Codex authorization flow have not been retested on Android; existing stores receive
  the change at kernel restart only when their old Tuxport profile is recognized and unedited.

- Notification authorization passed debug assembly, lint and API 34 checks: visibility after
  banner dismissal, first request, denial then system settings, and removal after granting in
  system settings and returning or directly in the permission dialog. Android 11–12, vendor
  settings and TalkBack remain unverified.

- Two shell-title tests and two existing bash/ash prompt/login tests passed on API 34, including
  bash command titles, pipelines, literal/control text, history fallback, status preservation and
  hook reloads. Host PTYs covered bash 5.2 and zsh, including history exclusions, existing DEBUG
  traps and single title emission after reload. Custom prompt frameworks, history changes in
  later PROMPT_COMMAND entries and live upgrades of already-running shells remain limited.

- Diagnostic export was exercised on API 34 with privacy off and with device credentials:
  success, cancellation, wrong PIN, screen-off invalidation and no replay after unlock.
  Save now authenticates before opening the picker: API 34 covered wrong PIN/cancellation before
  picker launch, successful save to Downloads with ZIP integrity, cancelled-picker cleanup,
  no repeat prompt on return while the shield stays locked, and orphan cleanup after process death.
  Debug/release build and lint and thirteen diagnostic/privacy tests passed. Real biometric
  hardware, cloud document providers and receiving the ZIP in third-party apps remain unverified.
- File-access consent on physical devices; API 34 covered English/Chinese layout, cancellation,
  Back/outside dismissal, recreation, enabling and immediate disabling; build and lint passed.
- About's version row and Play Store navigation on a device, including web fallback, missing
  handlers and returning to Settings; `assembleDebug` and `lintDebug` passed.
- About's Telegram, project and privacy-policy links and return to Settings on a device,
  including Custom Tabs, a browser without Custom Tabs and no URL handler; debug assembly
  and lint passed for the policy entry, but no Android device was connected for navigation checks.
- That Alpine's doas reads `/etc/doas.d/*.conf` in lexical order (the NOPASSWD file relies on coming
  after `20-wheel.conf`), and that doas reads only `*.conf` and sudo skips names with a dot (the
  temporary rules files rely on it).
- The profile's bash marks in the app on Debian (tried only in a Docker container with Ubuntu
  26.04's bash 5.3; on Alpine in instrumentation tests, the first login's install and the
  fallbacks included, and by closing a tab at its prompt; busybox ash's in an instrumentation test,
  a `~/.profile` prompt and a nested ash included), and with a
  `PROMPT_COMMAND` array or a prompt tool that rewrites PS1 after it each time.
- The profile's zsh marks in the app (tried only in a Docker container with Alpine's zsh 5.9 under
  a pty: the marks, a theme-like hook setting PS1, redraws on resizes), and with a right prompt
  (its cells follow B, so they count as input), a PS1 of several lines (rows after the first are
  not marked as continuation rows), `prompt_percent` unset, or a prompt framework (oh-my-zsh,
  powerlevel10k).
