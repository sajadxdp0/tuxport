# Terminals (kernel/tty) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/tty"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D8 were seeded by hand from the retired design doc
> §9 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: PTYs and the line discipline live in the kernel layer, not on a host PTY
- **What**: Every PTY pair, its termios and line discipline, and job control are implemented in process; the app's terminal tabs and `tuxport-run` talk to kernel-held masters.
- **Why**: With a host PTY, the host's `ISIG` would send signals to host process groups, which know nothing about guest process groups; guest programs (tmux, screen, `script`, sshd) also need to allocate their own PTYs through `/dev/ptmx`.
- **Tradeoffs**: Every line-discipline feature has to be reimplemented; a subset of termios flags is supported.
- **Watch out**: The defaults must stay those of Linux's pts `init_termios`.
- **Source**: design review 2026-10-24 (§9), round 05 (`05-m3b`)

### D2: Closing the last master hangs the terminal up, deferred to the BKL release
- **What**: As in Linux's `pty_close` → `tty_vhangup`, the session leader gets SIGHUP and SIGCONT, the session loses its controlling terminal, and open slave ends read 0 and write `EIO`; the work runs when the closing thread releases the BKL.
- **Why**: Only decrementing a count left programs running after their terminal was gone (opencode kept redrawing after `tmux kill-server` and held memory until the OOM killer ended the next CLI). `Drop` cannot reach the kernel, hence the deferral.
- **Tradeoffs**: A short window exists where the master is gone but the hangup has not run (slave I/O returns `EIO` then).
- **Watch out**: Every path that closes a master must leave the terminal for `hang_up_closed` or hang it up itself.
- **Source**: round 104 (`104-pty-hangup`)

### D3: `vhangup(2)` and `TIOCVHANGUP` hang up one side by generation
- **What**: Each side keeps a generation; bumping it hangs up every end opened before, while the node stays reopenable.
- **Why**: util-linux `login` calls vhangup to evict processes left on a terminal, and agetty `--hangup` and systemd use `TIOCVHANGUP`; LTP's vhangup tests were skipped.
- **Tradeoffs**: Only users running `login`/`agetty` themselves benefit; the app logs in without them.
- **Watch out**: Permission differs per call (`CAP_SYS_TTY_CONFIG` vs `CAP_SYS_ADMIN`), as in Linux.
- **Source**: round 130 (`130-vhangup`)

### D4: termios2 ioctls are supported, with speeds recorded only
- **What**: `TCGETS2` and the `TCSETS*2` family share the kernel termios2 layout; baud rates are stored and reported but do nothing.
- **Why**: glibc ≥ 2.42 implements `tcgetattr` (and so `isatty`) only with `TCGETS2` and does not fall back, so on Ubuntu 26.04 every terminal was "not a tty" and readline stayed off.
- **Tradeoffs**: None in practice; a PTY has no line speed.
- **Watch out**: Keep `TCGETS` and `TCGETS2` views consistent (speed bits recomputed from CBAUD).
- **Source**: round 145 (`145-termios2`)

### D5: Packet mode (`TIOCPKT`) is implemented
- **What**: Master reads carry a status byte (flush, stop/start, DOSTOP/NOSTOP, IOCTL) or `TIOCPKT_DATA` before data.
- **Why**: GNU screen sets packet mode on every window's master and refuses to open the window when it fails; telnetd and rlogind use it too.
- **Tradeoffs**: `EXTPROC` only reports; local editing is not disabled.
- **Watch out**: A pending status byte must be returned alone and wake pollers with `POLLPRI`.
- **Source**: round 146 (`146-pty-packet-mode`)

### D6: A new PTY starts like Linux's: blocking master, 0×0 window
- **What**: Before a slave has ever been opened, master reads block; `EIO` and `POLLHUP` only follow once all opened slaves closed. New terminals start at 0×0.
- **Why**: screen reads the master right after forking the window's child, before the child opens the slave, and treated the early `EIO` as the window ending; `script` with a non-terminal stdin expects 0×0 as on native Linux.
- **Tradeoffs**: none
- **Watch out**: Console terminals take their size from the host instead.
- **Source**: round 150 (`150-new-pty-state`)

### D7: Flow control and flushes follow Linux's directions
- **What**: `TCFLSH` on the master flushes the right queue, and `TCOOFF` stops slave output so slave writes block.
- **Why**: The reversed direction and non-blocking TCOOFF made CPython's test_termios and test_ioctl hang until their 20-minute timeouts.
- **Tradeoffs**: none
- **Watch out**: Output stopped by `TCOOFF` cannot be restarted by `^Q`.
- **Source**: round 75 (`75-pty-flow`); 8dc166b

### D8: The host console mimics `script` at end of input
- **What**: Each app tab and each `tuxport-run` session is a console with a new session leader; when the host's input ends, the bridge waits until the program has read what was sent and then sends `VEOF` instead of hanging up.
- **Why**: `printf 'make\n' | tuxport-run --pty sh -i` left the shell waiting for input forever; util-linux `script` behaves this way, and tests drive interactive shells through pipes.
- **Tradeoffs**: Programs that ignore EOF keep running until the first process exits.
- **Watch out**: An EOF still queued when the program leaves canonical mode must stay an EOF (D16).
- **Source**: rounds 05 (`05-m3b`), 102 (`102-snmp-pty-eof`); 117f927

### D9: PTYs, the line discipline, job control and the host console are one kernel/tty module
- **What**: kernel/tty covers `tty.rs`, `syscall/tty.rs` and `terminal.rs` (PTY pairs, the line discipline, termios2, packet mode, job control and the host console), while setsid/setpgid and exit-time signals stay in kernel/process and stop rules in kernel/signal.
- **Why**: The retired design doc's §9 summarised the terminal layer in five bullets that missed its limits and differences from Linux.
- **Tradeoffs**: Job control is split across three modules, and the map's Linux comparisons come from knowledge of the kernel sources rather than a checkout; host CLI integration belongs to harness/cli and notice packaging to android/build.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them one slave write overshooting the output limit by its expansion, `TIOCSCTTY` returning 0 to a non-leader, `TIOCSPGRP` error codes, console input accepted after its session ended and `/dev/tty` and `/dev/console` reporting a pts device number instead of 5:0 and 5:1.
- **Source**: 5d21cc4 (bug list since updated by 24ecd25, 4e64e74, 5d23297, 5d7b936, d9e1f0c, e5aae61 and ae1b507); 60790c14; 39e55b4b

### D10: TIOCGPGRP and TIOCGSID on a master skip the controlling-terminal check
- **What**: On a master, TIOCGPGRP and TIOCGSID answer without the ctty check (no session is still `ENOTTY` for TIOCGSID, no foreground group reads 0) and TIOCNOTTY is always `ENOTTY`, while the slave keeps the check, as on Linux.
- **Why**: Both PTY ends share one `Tty`, so the slave-side check also ran on the master, and tmux's window names and pane cwd depend on asking the master.
- **Tradeoffs**: Any holder of a master fd can read the slave's foreground group and session, exactly as on Linux.
- **Watch out**: TIOCSPGRP keeps its ctty check on the master, also as on Linux.
- **Source**: d8136b6

### D11: TIOCSTI follows Linux 6.1: one's own controlling terminal or `CAP_SYS_ADMIN`
- **What**: TIOCSTI returns `EPERM` unless the fd is a slave that is the caller's controlling terminal or the caller has `CAP_SYS_ADMIN`; a master is never a controlling terminal, and with the capability its byte goes to what the master reads, ahead of held output.
- **Why**: Typing into another session's terminal is a privilege-escalation path (a root shell reading a terminal that an unprivileged process still holds).
- **Tradeoffs**: Newer distribution kernels refuse TIOCSTI to everyone without `CAP_SYS_ADMIN`, while Tuxport keeps 6.1's rule for one's own terminal.
- **Watch out**: None known.
- **Source**: ef5f836

### D12: Terminal files read in the order their code runs
- **What**: `impl Tty` follows the module docs' data flow (input, slave read, output, master read, ioctl helpers) with outside accessors last; syscall/tty.rs runs allocation and opening, I/O, ioctls, job control and hangup; terminal.rs puts the consoles `Machine` calls in lifecycle order, then the tuxport-run bridge, then shared helpers.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; the terminal files were written helpers first.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; Default impls for `Termios` and `Winsize` stay before the inherent impls, and terminal.rs's shared helpers section holds one function.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 688efa0

### D13: The line discipline is a port of Linux's n_tty
- **What**: Input waits in a 64 KiB `unprocessed` buffer (Linux's flip buffer) and enters the 4095-byte line discipline as it has room, also on slave reads and termios changes, with n_tty's special-character order, ECHOPRT and column-aware erase, output processing with the cursor column, an echo queue committed per batch, lines carrying their VEOF mark, queued input merged on mode switches and TCFLSH as `pty_flush_buffer`.
- **Why**: Starnix's recorded PTY scenarios showed the line discipline differing from Linux in 37 cases (missing ECHOPRT, ONOCR, ONLRET, OLCUC, XTABS, IUCLC, ISTRIP, PARMRK and EXTPROC handling, erase echo without columns, ECHOCTL rules, flushes, packet status, input lost on mode switches).
- **Tradeoffs**: Signal characters discard output Linux may already have passed on, IMAXBEL, HUPCL, VDISCARD and flow-control lookahead stay unimplemented, and master writes now block at 64 KiB of unprocessed input instead of 4096 bytes.
- **Watch out**: Programs that relied on the old leniency (TIOCSTI past a full buffer, TCIOFF under TCOOFF) see Linux's behaviour, and input processed on slave reads sends its signals from the reader's thread.
- **Source**: 24ecd25, 8dc166b

### D14: Taking a terminal needs read access, and stealing one needs `CAP_SYS_ADMIN`
- **What**: TIOCSCTTY takes another session's terminal only with argument 1 and `CAP_SYS_ADMIN` and needs `CAP_SYS_ADMIN` through an fd without read access, and opening a slave acquires it only for reading opens, with readability passed in by the vfs (kernel/vfs D26).
- **Why**: Any session leader holding an fd to a terminal, even a write-only one or the stdin a sandbox inherits, could make it its controlling terminal and read its input and signals.
- **Tradeoffs**: `CAP_SYS_ADMIN` counts only in the initial user namespace, as Linux's `capable()` does, so container root cannot steal terminals either.
- **Watch out**: `OpenFile::readable` treats access mode 3 as readable where Linux's `FMODE_READ` does not, so an ioctl-only open still takes a terminal.
- **Source**: 838b5b4

### D15: The parts of the line discipline adapted from Starnix keep its notice
- **What**: tty.rs's module docs name the items adapted from Starnix's `lib/line_discipline` (`Echo` and `commit_echoes`, erasing and `echo_erased`, `output_char`), the Starnix commit 86962cb and The Fuchsia Authors' notice, pointing to `android/licenses/fuchsia-LICENSE`, and the map's gotchas say the notice moves with that code.
- **Why**: The echo queue, erasing and output processing rewritten for D13 closely follow Starnix's BSD-licensed code, whose license requires keeping its copyright notice, and tty.rs did not name it.
- **Tradeoffs**: tty.rs mixes Tuxport's AGPL code with BSD-derived parts, and the notice is kept by hand with nothing checking it.
- **Watch out**: Moving those functions without the notice, or keeping it after a rewrite that no longer derives from Starnix, makes the attribution wrong, and the item list and year range are written by hand.
- **Source**: f2d3264

### D16: The console's end of file stays an EOF when the program leaves canonical mode
- **What**: `Tty::inject_eof` queues an empty line ended by VEOF and marked `injected` when the terminal is canonical with nothing being edited (otherwise VEOF goes in as typed input), and `set_termios` turns a marked EOF into the old VEOF character instead of a NUL when leaving canonical mode, so the bridge sends its EOF once (D8).
- **Why**: Since the n_tty port (D13), leaving canonical mode turned the queued EOF mark into a NUL as Linux does, which a shell's line editor inserted into its line, so `sh -i` fed through a pipe never exited, and the bridge's resend came too late.
- **Tradeoffs**: The console's EOF behaves unlike a VEOF typed in canonical mode on Linux, which stays a NUL, on purpose, while util-linux `script` keeps Linux's behaviour and has the same hang.
- **Watch out**: A program that leaves canonical mode with other lines queued before the EOF gets their data followed by the VEOF character, as it would have from the old resend.
- **Source**: 117f927

### D17: Non-canonical reads and readiness follow `input_available_p`, and `VTIME` is an inter-byte timer
- **What**: A nonblocking non-canonical read returns whatever input is there and `poll_readable` waits for `VMIN` bytes only when `VTIME` is 0, as Linux decides for both read and poll; with `VMIN` above 0 `slave_read` restarts the `VTIME` deadline whenever the raw bytes it has seen change and clears it when none are left (another reader or a flush took them), while `VMIN` 0 times from the start of the read.
- **Why**: A nonblocking read below `VMIN` returned `EAGAIN` while `POLLIN` ignored `VMIN`, so event loops spun, and `VTIME` timed the whole read from its first byte, unlike `n_tty_read`, cutting short input that arrived in a slow steady stream.
- **Tradeoffs**: Bytes stay queued until the read returns, so another reader can still take them where Linux's reader has already copied them, and a deadline passing just as a byte arrives returns what is queued where Linux would copy it and wait again.
- **Watch out**: None known.
- **Source**: 5d23297, ae1b507

### D18: The flushing and line ioctls check for background callers, and a leader's exit sends only SIGHUP
- **What**: TCFLSH, TCXONC, TCSBRK, TCSBRKP and TIOCSETD run the background check, and `release_ctty` at a session leader's exit sends only `SIGHUP`.
- **Why**: Background jobs were not stopped for these ioctls, and a session leader's exit with a PTY as its controlling terminal sent an extra `SIGCONT`, unlike Linux.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D19: Echoes respect the output limit, and held echoes go out ahead of the next slave write
- **What**: `commit_echoes` outputs an echo only when it fits whole and keeps at most 4096 waiting (the oldest dropped), `send_xchar` drops its character on a full queue, and `slave_write` commits held echoes through `Tty::echoes_before_write` at the top of each pass, before measuring the room for its data, and wakes the master when echoes or data went out, as `n_tty_write` starts with `process_echoes` (reading the master sends none, as on Linux).
- **Why**: Echoes and `send_xchar` ignored the output limit, so a master that never read let the output grow without bound, and echoes held by a full queue then waited for the next input behind data the slave wrote meanwhile, so characters typed during a flood showed up after the next prompt.
- **Tradeoffs**: One slave write can still overshoot the limit by its output expansion, and a blocked write sends held echoes before each piece it sends after waiting, where Linux processes them once per 2048-byte chunk.
- **Watch out**: A zero-length write sends held echoes only if it reaches `slave_write`.
- **Source**: 5d7b936, d9e1f0c (held echoes), e5aae61

### D20: Terminal ioctls Tuxport does not know are counted as unimplemented paths
- **What**: `tty::ioctl`'s default arm counts the request on a terminal in kernel/signal's table of unimplemented paths (kernel/signal D24) before returning `ENOTTY`.
- **Why**: Terminal ioctls Tuxport does not know ended in an unrecorded `ENOTTY`.
- **Tradeoffs**: The `EIO` and `ENOTTY` of hung-up terminals are not counted.
- **Watch out**: None known.
- **Source**: 99b4216

### D21: A failed host spawn gives its console back
- **What**: `terminal::detach` clears the console's session and foreground group when they are the failed leader's and restores whether a session was ever attached, so the console's end of session is reported as before (kernel/process D53).
- **Why**: The console a failed spawn had attached kept that process as its session, so every later attach was EBUSY.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 143bb86

### D22: The app reads a console's foreground group while a program's report lasts
- **What**: `Machine::console_foreground` (`terminal::fg_pgrp`) returns the console's foreground group under the BKL, `ESRCH` when there is none, and a tab's pump calls it only while a progress or status report lasts (android/terminal D39).
- **Why**: The app must know when the program that reported progress or a status has given the terminal back, and prompt marks, which would tell, come only from shells that send them.
- **Tradeoffs**: One more BKL acquisition per console read while a report lasts, and the group id is the kernel's own, not translated into any pid namespace.
- **Watch out**: A tab streaming output while its program reports (an AI CLI at work) contends for the BKL; none was observed.
- **Source**: 22c8ab0

### D23: The app asks whether a console reads a password by its termios
- **What**: `terminal::password_input` (ICANON set, ECHO clear), wrapped by `Machine::console_password_input` under the BKL, tells the app when a tab's console reads a password, for the lock cursor and the IME's password mode (android/terminal D48).
- **Why**: Only the kernel layer knows a console's termios, and Ghostty's rule for its secure-input lock is the same pair of flags.
- **Tradeoffs**: A BKL acquisition per pump read and every 200 ms while a lock shows, and termios changes raise no event, so the app finds them only when it asks.
- **Watch out**: Programs that read passwords in non-canonical mode (masking line editors) show no lock.
- **Source**: 2b04a43

### D24: Host console reads preserve foreground and background writer runs

- **What**: Slave writes record ranges from non-foreground process groups, and `read_console_run` stops at writer changes while treating echoes as foreground; ordinary console and guest-master reads stay unchanged.
- **Why**: Only the kernel can tell a background job’s bytes from shell output at a prompt.
- **Tradeoffs**: Alternating writers shorten host reads and use a deque bounded by the 64 KiB output queue.
- **Watch out**: Jobs without job control share the shell’s group, and a shell writing before reclaiming the terminal is classified as background; native interpretation belongs to android/vtcore.
- **Source**: daab4446; 20137e4c; 81f60bef; d053be24; 39e55b4b

## Superseded

- ~~Output stopped by ^S kept in the queue behind a mark (`held_from`, D7 in part, round 75)~~ → replaced by **D13** in 8dc166b (2026-11-07)
- ~~The line-discipline subset described at the module split (5d21cc4)~~ → replaced by **D13** in 8dc166b (2026-11-07)
- ~~The bridge resends `VEOF` when the program left canonical mode before reading it (D8 in part, rounds 05, 102)~~ → replaced by **D16** in 117f927 (2026-11-07)
- ~~`VTIME` with `VMIN` above 0 is a total timer from the first byte (D17's tradeoff, 5d23297)~~ → replaced by **D17** in ae1b507 (2026-11-10, no SUPERSEDES declared)
- ~~Echoes held by a full queue wait for the next input and go out behind a slave write's data (D19's watch-out, 5d7b936, d9e1f0c)~~ → replaced by **D19** in e5aae61 (2026-11-10, no SUPERSEDES declared)
