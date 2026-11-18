# 12. Threat model

Like iSH and Termux, Tuxport is **not a security sandbox**: it promises no security boundary between different uids inside the guest, nor between the guest and the host app. The requirements below are numbered continuously across subsections.

## 12.1 Hard engineering invariants

1. **The guest can reach memory only through its own page tables**: the DBT copies only whitelisted instructions verbatim, and every memory-access instruction must be rewritten ([engine map](../../.agents/maps/engine.md)).
2. **The kernel layer never treats a guest pointer as a host pointer**: addresses in syscall arguments are always `u64` or `UserSlice` (`core/kernel/src/uaccess.rs`) and can only be read or written through `Mm`.
3. **Host paths are never built from guest input**: only paths of the form `objects/<number>` appear on the host side, and symlinks exist only in the database ([kernel/store map](../../.agents/maps/kernel/store.md)). Problems like "a malicious tarball escaping through a symlink" are therefore structurally impossible.

## 12.2 Robustness

4. **The guest's own OOM killer**: all guest memory has a total limit; when it is exceeded, Tuxport picks one guest process to kill, instead of letting the system LMK kill the whole app. Implemented in `core/kernel/src/oom.rs`.
   - **Limit**: what is measured is the anonymous memory of the whole app process (RssAnon + RssShmem + VmSwap, which is what LMK looks at), minus one copy of the Tier 1 code-cache pages that are still mapped in the write view (W^X in the [engine map](../../.agents/maps/engine.md)). The default is half of physical memory; `MachineOptions::memory_limit` changes it.
   - **Checking**: new pages the guest touches are handled by the host kernel, so the check is periodic: every 20 ms while any guest thread is running, and every 5 ms once usage passes 3/4 of the limit.
   - **When exceeded**: one process, picked by Linux's oom_badness, is ended with SIGKILL and recorded in the kernel log ([kernel/signal map](../../.agents/maps/kernel/signal.md)); Tuxport waits for it to finish exiting (up to 1 second) before looking again.
   - **cgroup**: the limit and the usage are also shown to the guest as `memory.max` and `memory.current` ([kernel/cgroup map](../../.agents/maps/kernel/cgroup.md)); a `memory.max` the guest sets on a child cgroup is checked too.
   - **Fork bombs**: stopped by the default `threads-max` and `RLIMIT_NPROC` ([kernel/process map](../../.agents/maps/kernel/process.md)).
5. **A bug in the kernel layer or the engine ends only the guest process that hit it**: the kernel layer is compiled with `panic=unwind`; after catching a panic it ends the current guest process and produces a report. Implemented in `core/kernel/src/bug.rs`.
   - **Run loop**: every entry into the kernel (including syscalls that do not hold the BKL) catches panics with `catch_unwind`, and acquiring the BKL ignores the poison flag; engine bugs go through `TUXPORT_EXIT_HOST_FAULT` in the [engine map](../../.agents/maps/engine.md).
   - **Ending and reporting**: in both cases the whole process is ended with SIGSEGV, like a Linux oops, and a report (with the panic message and location) is written to stderr (logcat on Android); the same report goes into the kernel log, and the app explains what happened ([android/service map](../../.agents/maps/android/service.md)).
   - **Outside the run loop**: an error at thread start, in any step of exit, or while the timer thread handles one process's timers ends that process; each of the nine steps of process teardown is caught separately, so an error in one step loses only what that step does; an error in the memory-limit check or in signal-driven I/O turns that feature off; a host call into a `Machine` method (JNI) fails only that call.
   - **Debug builds**: abort the host process outright by default; `TUXPORT_ON_BUG=kill`/`abort` overrides this.

## 12.3 Interfaces exposed to Android

6. **Only the launcher Activity and the DocumentsProvider are exported**: there is no intent that lets other apps run commands. The DocumentsProvider is protected by `MANAGE_DOCUMENTS`, so only the system Files app can call it directly; other apps go through URI permissions the user grants.
7. **A notification the first time the guest `listen`s on a port on a non-loopback address**: the kernel queues the events (`Machine::take_listens`), and the app takes them every 2 seconds and posts a notification in the "Network services" channel with the process name, the port, and the host port a low port was moved to ([kernel/net map](../../.agents/maps/kernel/net.md)).
8. **JIT code pages use a W^X double mapping** ([engine map](../../.agents/maps/engine.md)).

## 12.4 Trust chain and privacy

`apk` verifies every package's signature with the Alpine public keys shipped in the rootfs, so switching mirrors does not weaken trust. Debug logs stay local; if telemetry is ever added, it must be opt-in, and Play's Data safety form must be updated with it.
