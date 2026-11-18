# kernel/vdso Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/vdso.md` for the paired decision history (not yet created; pre-split decisions D2, D4 and the vDSO portion of D12 stay in `.agents/decisions/kernel/signal.md`).
>
> Verified: 2026-11-15 (split out of the kernel/signal full mapping of 2026-11-03; the boundary was checked against the source at the split)

## Responsibilities

`kernel/vdso` owns `core/kernel/src/vdso.rs`: choosing and validating a host vDSO image,
building the fallback ELF, the fixed signal-return trampoline and clock-stub detection.
The cached `Image` is a static `OnceLock`, shared for the host process's lifetime.

Mapping and auxv remain `kernel/exec`/`kernel/mm`, time-namespace transitions `kernel/ns`,
clock syscalls `kernel/time` and fast-path dispatch `kernel/process`. Signal frames and return
validation stay `kernel/signal`.

## Key types

- `vdso::Image` — the image every process maps, chosen once per host process: its bytes, the same with svc
  stubs at the clock functions' entries, the sigreturn offset, the clock functions' offsets with
  their syscalls, and for the host's vDSO a `Vvar` (the length of `[vvar]`, the host address of
  its start, and how many bytes of it the guest may read).

## Public entry points

- `image`, `svc_image`, `host`, `vvar`, `clocks`, `svc_bytes`, `sigreturn_offset`.
- `clock_stub` and `clock_stub_at` identify calls replacing userspace clock reads.

## Data flow / lifecycle

### vDSO

- The image is chosen at its first use and kept for the host process's life. Preferred is the host
  kernel's own vDSO: a copy of the host's `[vdso]` mapping (found through `AT_SYSINFO_EHDR` and
  `/proc/self/maps`), whose clock functions read the counter (`CNTVCT_EL0`, or `CNTVCTSS_EL0` on
  hosts with ECV, both read natively by the engine) and the host's vvar data, so `clock_gettime`,
  `gettimeofday` and `clock_getres` run in guest code without entering the kernel layer and return
  exactly what the host's syscalls return; what they leave to the kernel (CPU-time clocks, invalid
  clocks, a host whose counter is unusable) is a real syscall, as on Linux. The loader maps the
  host's vvar pages read-only as `[vvar]` right below the copy, at the distance the host's code
  expects (`kernel/mm`): the guest can read the first host page, or the first two when the host
  itself runs in a time namespace (its first page's clock mode is `VDSO_CLOCKMODE_TIMENS`; the
  real data is then on the second).
- The copy is used only if musl's lookup finds the four symbols with version `LINUX_2.6.39`,
  `__kernel_rt_sigreturn` is exactly `mov x8, #139; svc #0`, and each clock function is at least
  12 bytes long. It is changed only where Tuxport's contract needs it: every other defined symbol is
  made undefined (newer kernels' `__kernel_getrandom` reads vvar pages the guest does not get), and
  the `Linux` version note says 6.1.0. Its clock functions then run once in the interpreter, in a
  private address space with the vvar data mapped as in a guest, and every result must lie between
  host readings taken just before and after (realtime, monotonic, raw, both coarse clocks,
  boottime, TAI), with the same resolutions and timezone; syscalls they make are answered by the
  host, and anything else, or more than 100000 instructions, fails it.
- Otherwise (no host vDSO, a check failing, a non-arm64 build) the image is built at runtime as a
  one-page minimal real ELF shared object (`ET_DYN`, a loadable segment and a dynamic segment,
  `DT_HASH`, symbol versions with `LINUX_2.6.39`, soname `linux-vdso.so.1`), with
  `__kernel_rt_sigreturn` preceded by a `nop` and the clock functions as `mov x8, #nr; svc #0; ret`
  stubs. With `TUXPORT_LOG=1` the reason the host's was not used is logged.
- Either is passed through `AT_SYSINFO_EHDR`. Each process maps a private anonymous copy (debuggers
  can set breakpoints in it); `MADV_DONTNEED` over it brings back the image (`kernel/mm`), as
  Linux's special mapping faults its pages back.
- In a time namespace with offsets the host's code would return the host's clocks, so the clock
  functions' entries of the host's image are overwritten with the same svc stubs (`Mm::vdso_syscalls`)
  when such a namespace is entered (`kernel/ns`), and the kernel layer adds the offsets; this
  is never undone. An svc from a stub (the svc right after a clock entry, with that entry's
  syscall) takes the no-BKL fast path and skips ptrace and seccomp stops, but only for
  `gettimeofday` and the clocks Linux's vDSO reads in user space (realtime, monotonic, raw, the two
  coarse clocks, boottime, TAI); other clocks, and every syscall the host's code makes, are
  ordinary syscalls that tracers and seccomp see, as on Linux.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/signal` uses the sigreturn offset; `kernel/exec` maps the image and sets auxv.
- `kernel/mm` restores discarded image pages and installs clock stubs for `kernel/ns`.
- `kernel/process`, `kernel/ptrace` and `kernel/cred` use clock-stub classification to avoid
  tracing/filtering calls that replace userspace clock reads.

### Outbound

- `hal` discovers the host vDSO/vvar and supplies reference clock/timezone calls.
- `engine` Cpu/Mmu interpret the host image in a private address space for validation.
- `kernel/signal` supplies debug logging and the Linux ABI contract.

## Invariants and gotchas

- The host's vDSO code is run unchanged except for its symbols and notes: the engine must execute
  whatever the host kernel put there (alternatives such as ECV's counter included), which the
  check at first use guards; the time-namespace detection relies on the clock mode being the
  second word of the host's vvar data (Linux 5.6 on).
- The sigreturn trampoline must remain `mov x8, #139; svc #0`; `kernel/signal` frames and
  unwinders depend on its bytes.
- Clock-stub detection must agree with mapping offsets and syscall/ptrace/seccomp call sites;
  real syscalls from the host image remain visible to tracers and filters.

## Open questions

None.
