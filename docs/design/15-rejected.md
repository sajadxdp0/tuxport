# 15. Rejected approaches

| Approach | Why it was rejected |
|---|---|
| Native arm64 execution + intercepting `svc` (seccomp `RET_TRAP`/SIGSYS, or binary rewriting) | Violates Play's policy on executing downloaded code; a guest changing `TPIDR_EL0` breaks bionic/ART's TLS; fork cannot be implemented in a shared address space |
| Syscall User Dispatch | On arm64 it depends on the Generic Entry conversion, a patch series still under review in early 2026 and absent from existing GKI devices |
| Multiple host processes (really forking host processes, proot-style) | Subject to the phantom process killer; no different from Termux + proot-distro |
| fastmem fixed slots (`host = base + guest`) | Limits the number of processes under a 39-bit VA; a 4 GB slot cannot hold V8's cage; the VMA count hits its ceiling; the guest page size is forced to equal the host page size; there is no CoW primitive within a host process |
| SIMD&FP semantics in Tier 0 in pure C (plus SoftFloat) | Hundreds of instruction forms, each of which must match the hardware's NaN, rounding and flag rules exactly: tens of thousands of lines; executing the single instruction natively on a same-ISA host is bit-exact for free |
| dynarmic as the core | Its page tables do not distinguish read from write permission; memory callbacks cannot get precise guest state; the code cache cannot be shared across instances; the same-ISA case still detours through an IR; the upstream repository is now a 404, and the active fork cares only about A32 |
| QEMU / Unicorn | GPLv2 (some QEMU files allow only GPLv2), which cannot be combined with Tuxport's AGPLv3 or with the GPLv3 and Apache-2.0 code in the app; QEMU linux-user's model is one host process per guest process |
| A single host thread + green threads / M:N | The former cannot run anything in parallel, which makes `make -j` pointless; the latter carries the complexity of both sides |
| iSH fakefs-style (real directory tree + metadata database) | Android cannot create hard links; keeping the directory tree and the database consistent across rename/unlink is hard |
| Disk image + in-process ext4 driver | Needs its own journaling for crash consistency; fixed capacity; data cached twice; cannot share the host page cache |
| Borrowing a host PTY | Signals raised by the host's `ISIG` cannot reach the guest process group |
| xterm.js / writing our own terminal renderer | The former has poor IME behavior and latency on Android; the latter is too much work |
| `MANAGE_EXTERNAL_STORAGE` | Play does not approve it for terminal apps |
| `dataSync` foreground service | From Android 15 it may run for at most 6 hours in every 24 |
| Pretending to be root everywhere (proot `-0`-style) | Breaks postgres, test suites, sshd privilege separation and the like |
| Everything in C | The kernel layer is most of the code, so that is where memory safety pays off most |
| Implementing io_uring | Few programs use it: Android's seccomp forbids io_uring to apps, so programs on phones need a fallback anyway; Go, Java, Python, the Rust standard library and tokio do not use it, and Node.js's libuv has not used it by default since 1.48. Doing only part of it is more dangerous than ENOSYS: a program that detects it as usable stops taking its fallback. It would also need a "don't wait" mode in the kernel's wait mechanism, which every future syscall would have to account for |
| Handing the BKL-holding part to a dedicated thread (coroutines, an event loop) | Only serializes in a different way, and without contention adds a cross-thread round trip to every syscall ([kernel/process map](../../.agents/maps/kernel/process.md)) |
