# kernel/exec Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/exec.md` for the paired decision history (decisions from before the split are D7, D17, D31, D41, D43, D46 and D51 in `.agents/decisions/kernel/process.md`).
>
> Verified: 2026-11-11 (split out of the kernel/process full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/exec` replaces a process's program (execve(2), 6.1's `fs/exec.c` and `fs/binfmt_elf.c`):

- execve and execveat: the argument limit, the target (path, fd, `/proc/<pid>/fd/<n>`,
  `/proc/<pid>/exe`), `#!` scripts, `ETXTBSY`, and the credential effects of set-id bits and file
  capabilities;
- the ELF loader: checks before the point of no return, doomed images after it, segment mapping,
  placement, the initial stack and auxv;
- the point of no return: what the process gives up and resets, in order;
- the process's `exe_file`, the cache of opened executables, and the names of the program's and
  interpreter's mappings;
- loading the first program of a host-started process (`Machine::spawn` through `loader::load`).

Root: `core/kernel/src/loader.rs` and the execve section of `core/kernel/src/syscall/proc.rs`
(`execve`, `execveat`, `exec`, argument reading, `#!` parsing, file capabilities, load errors,
`interp_label`/`label_image`, `kill_other_threads`).

Outside the module: thread groups, the wait used while other threads die, vfork completion,
the run loop that re-attaches the translator (`kernel/process`), POSIX timers (`kernel/time`)
and host spawn (`kernel/boot`); the credential
transition and file capabilities (`kernel/cred`); entering the time namespace for children
(`kernel/ns`); building and laying out the address space (`kernel/mm`); signal handler reset,
forced fatal signals (`kernel/signal`), the ptrace exec event (`kernel/ptrace`) and the vDSO image (`kernel/vdso`); close-on-exec and
writer checks (`kernel/files`); noexec and nosuid (`kernel/mount`); FUSE programs (`kernel/fuse`).

## Key types

- `loader::Executable`, `ExeCache` (32 opened executables, least recently used dropped), `Args`,
  `Prepared` (the hand-off between the locked and unlocked exec phases), `Image` (with `doomed`);
  `process::ExeFile` (the process's `exe_file`: path, store mount and inode, dentry, host file, and
  for a program found through an overlay its overlay node, held while recorded).
- `loader::LoadError` — why loading failed, turned into an errno by `load_error`.

## Public entry points

- **Syscalls**: `execve`, `execveat`.
- **Kernel-internal**: `loader::prepare`/`load_prepared` (the locked and unlocked phases),
  `loader::load` (`Machine::spawn`), `loader::arg_limit`; `interp_label` and `label_image` (the
  names of the program's and interpreter's mappings in `/proc/<pid>/maps`).

## Data flow / lifecycle

### execve

- Arguments: the limit comes from `RLIMIT_STACK` (`bprm_stack_limits`: a quarter of the limit, at
  most 6 MB, at least 128 KB); a string of 128 KB or more or a larger total is `E2BIG`; an empty
  argv becomes `[""]`. `execveat` takes only `AT_EMPTY_PATH` and `AT_SYMLINK_NOFOLLOW` (else
  `EINVAL`; a symlink not followed is `ELOOP`).
- Target: by path, by fd (`AT_EMPTY_PATH`), through `/proc/<pid>/fd/<n>` (the open file) or
  `/proc/<pid>/exe`. It must be a regular executable file (`EACCES`), not on a noexec mount;
  nosuid is noted (memfds ignore both). A program (or interpreter) on a FUSE mount is read whole
  into a host copy by a retry before anything changes (`kernel/fuse`), runs as if from a nosuid
  mount, and carries its FUSE key as inode number.
- `#!` scripts: the interpreter line is parsed (up to 256 bytes) and the interpreter resolved
  relative to the cwd; five nested scripts run, a sixth is `ELOOP` (Linux 6.1's `exec_binprm`); a
  script run by fd through a close-on-exec directory is `ENOENT`.
- `ETXTBSY` if a writable open file refers to a `#!` script being run (checked before its
  interpreter is opened) or to the final executable (memfds never count as writers).
- Credentials: setuid and setgid (the latter with group execute) apply unless `no_new_privs`,
  nosuid, or the owner/group are unmapped; file capabilities come from the xattr; a tracer without
  `CAP_SYS_PTRACE` or a fs context shared with another process makes the execution unsafe, so it
  gains nothing (`kernel/cred`). A secure exec clamps the stack limit to 8 MB (written back to
  `RLIMIT_STACK`).
- Under the BKL the main ELF is parsed and the interpreter file opened, read and its headers
  checked; then the BKL is released while a new address space (sharing the kernel's read-only file
  mappings, the vDSO image, host random bytes) and initial stack are built. Back under the BKL, a
  killed thread gives up with `EINTR`. What Linux fails to map only after its point of no return
  makes the image doomed: execve commits as usual and then kills the process with a forced
  `SIGSEGV` (`force_fatal_sig`); such an image may keep partial mappings and has no stack.
- Point of no return, in order: the other threads are killed and waited for (the BKL is released
  while waiting; only the caller being killed can still fail it, with `EINTR`) and a non-leader
  takes over the tgid; enter the time namespace for children; unshare and close-on-exec the fd
  table (releasing locks, message-queue registrations, FUSE files and unix sockets of the closed
  fds); sample RSS; record the new `exe_file`; apply dumpable and pdeathsig rules; set the address
  space's user namespace (`ThreadGroup::mm_user_ns`, `kernel/cred`); clear `READ_IMPLIES_EXEC` from
  the personality (arm64's `SET_PERSONALITY`) and on a privileged exec the other
  `PER_CLEAR_ON_SETID` flags (`kernel/cred`); commit the credentials; clear POSIX timers and AIO;
  unshare and reset signal handlers; release the old address space (robust list,
  `clear_child_tid`, vfork) and install the new one with the CPU state reset, reaping shm
  segments marked for removal it held last; reset the alternate stack and comm; set PC and SP;
  force the `SIGSEGV` of a doomed image; re-kick if interrupted. The run loop then re-attaches the
  translator.

### ELF loader

- Before the point of no return, as 6.1's `load_elf_binary`: the program must be a 64-bit
  little-endian aarch64 `ET_EXEC` or `ET_DYN` with 1 to 65536 bytes of 56-byte program headers
  (`ENOEXEC`); only the first `PT_INTERP` counts, of 2 to 4096 bytes ending in NUL (`ENOEXEC`;
  contents past the end of the file `EIO`), its path cut at the first NUL (an empty one is
  `EACCES`, as opening the working directory is); the interpreter (ld-musl, or glibc's ld.so) must
  parse as an aarch64 ELF (`ELIBBAD`), and its own `PT_INTERP` is ignored.
- After it (the image doomed): a `PT_LOAD` with file contents whose offset disagrees with its
  address within the page, whose file size exceeds its memory size, or that reaches beyond 2^48 or,
  once placed, above the stack area (2^47 − 8 MB); an interpreter of another type than `ET_EXEC`
  or `ET_DYN` or with nothing to load; any failure to map; an entry beyond 2^48. A `PT_LOAD` of no
  memory still goes through these checks (and can give `AT_PHDR`) but maps nothing, a program
  without `PT_LOAD` loads (and faults at its entry), and contents past the end of the file are
  mapped (reading them is `SIGBUS`).
- `AT_PHDR` is the address of `e_phoff` in the last `PT_LOAD` whose file contents hold it, else 0,
  plus the bias (`PT_PHDR` is not used). `AT_BASE` is the interpreter's bias, 0 for an `ET_EXEC`
  interpreter, which loads where it says.
- A position-independent main program loads at `0x5555_5555_0000` (at its own addresses when
  they start above that); the interpreter and then the vDSO are placed top-down below the mmap
  area (so the vDSO is below the interpreter), with the host's `[vvar]` right below the vDSO when
  the vDSO is the host's (`kernel/vdso`). There is no ASLR (`engine`'s shared translations rely
  on that).
- Segments are mapped in order, each over what earlier ones mapped, as since 6.7's `elf_load`
  (which the differential tests' native side runs; 6.1 maps a page of the file even for a
  segment without file contents when its address is not page-aligned): the pages holding its file
  contents as a private mapping of the host object, the rest of its last file page zeroed when it
  has more memory than file, and its remaining pages anonymous memory that is readable and
  writable, and executable when the segment is (`vm_brk_flags`).
- The initial stack holds argv, envp, the execfn, the platform string and 16 random bytes, and the
  auxv in this order: `AT_SYSINFO_EHDR`, `AT_MINSIGSTKSZ`, `AT_HWCAP`, `AT_PAGESZ` (4096),
  `AT_CLKTCK` (100), `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_BASE`, `AT_FLAGS`, `AT_ENTRY`, `AT_UID`,
  `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_SECURE`, `AT_RANDOM`, `AT_HWCAP2`, `AT_EXECFN`, `AT_PLATFORM`
  (`aarch64`), `AT_NULL`. IDs are mapped into the process's user namespace. Building it checks the
  argument limit again, counting the pointers, auxv and strings, so it can be `E2BIG` where reading
  the strings passed.
- The main program's code and data bounds (Linux 6.1's `start_code`, `end_code`, `start_data`,
  `end_data` from its segments) are recorded in the address space for `/proc/<pid>/stat`.
- A Go build-ID note in the main program selects the translator's Go register set (`engine`;
  `TUXPORT_DBT_REGS` overrides it).
- Opened store and overlay executables are cached (32 entries, checked against the host file's
  device, inode, ctime and size; forgotten when rehomed, cleared under memory pressure).
  `Machine::spawn` loads through `loader::load`.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher (`kernel/process`): execve and execveat; a successful execve returns
  before the post-syscall sweeps and `Mm::sync`.
- `Machine::spawn` (`kernel/boot`): `loader::load` for a host-started process.
- `kernel/procfs`: `/proc/<pid>/exe` and `/proc/<pid>/fd/<n>` as targets, the mapping names.

### Outbound

- `kernel/process`: killing and waiting for the other threads, the tgid takeover and
  `leader_exit`, vfork completion (`release_mm`), `fast_changed`, `ExeFile`.
- `kernel/time`: POSIX timer state cleared at execve.
- `kernel/mm`: `Mm::new`, `share_files`, `into_shared`, `set_layout`, `mmap` and `munmap`
  (segments), `map_anonymous`, `map_vvar`, `find_free` (interpreter and vDSO placement),
  `set_brk`, `map_stack`, `set_start_stack`, `label`, `write_kernel` (segments, `.bss`).
- `kernel/cred` (set-id bits, file capabilities, unsafe execs, `mm_user_ns`, personality),
  `kernel/ns` (the time namespace for children and its vDSO stubs), `kernel/signal` (handler reset,
  `force_fatal`), `kernel/ptrace` (exec event), `kernel/vdso` (image), `kernel/vfs`, `kernel/files` and
  `kernel/mount` (resolution, noexec and nosuid, writers, close-on-exec), `kernel/fuse`
  (`open_fuse`, the close-on-exec flush), `kernel/aio`, `kernel/ipc` and `kernel/unix` (released at
  exec), `engine` (the Go register set).

## Invariants and gotchas

- execve changes no process state before killing the other threads: a FUSE retry runs the handler
  again from the start (`kernel/process`'s dispatcher), so everything before the last walk must be
  repeatable.
- The unlocked build of the new image touches only the new address space and structures that lock
  themselves (the kernel's file mappings), and re-checks after re-taking the BKL whether the thread
  was killed meanwhile.
- A doomed image commits like any other and dies only afterwards, so callers that skip the commit
  (`Machine::spawn`) must check `Image::doomed` themselves.

## Confirmed bugs / technical debt

- **`ETXTBSY` misses the ELF interpreter and nested scripts:** the script itself and the final
  program are checked, but running a program whose `PT_INTERP` interpreter is open for writing, or
  a script whose interpreter is another script open for writing, succeeds where Linux returns
  `ETXTBSY`.
- **Secure exec caps arguments at 2 MB:** the final argument check uses the clamped 8 MB stack limit,
  whereas Linux 6.1 computes the argument limit before clamping (up to 6 MB).
- **Doomed images:** `Machine::spawn` ignores `Image::doomed` and starts the program at pc 0 and
  sp 0 instead of failing the spawn; execve reports its ptrace exec event (and a successful syscall
  exit) for a doomed image, where Linux fails the exec before the event.

## Open questions

- Should a successful execve run the post-syscall sweeps and `Mm::sync` that it now skips?

## To verify

- The Linux 6.1 comparisons above that rest on kernel behaviour rather than repository source
  (`deny_write_access` on scripts and interpreters, argument limits under secure exec,
  `load_elf_binary` without `PT_LOAD`) were checked from knowledge of the kernel, not against its
  source.
