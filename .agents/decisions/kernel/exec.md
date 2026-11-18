# Program execution (kernel/exec) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/exec"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on execve and the ELF loader made before kernel/exec was split from kernel/process keep their `kernel/process` tags and live in `.agents/decisions/kernel/process.md`: D7, D17, D31, D41, D43, D46 and D51.

## Active

### D1: kernel/exec is split out of kernel/process
- **What**: kernel/exec owns `loader.rs` and the execve section of `syscall/proc.rs`: arguments, targets, `#!` scripts, `ETXTBSY`, the effects of set-id bits and file capabilities, the ELF loader with doomed images, the point of no return, `exe_file`, the executable cache and the names of the program's mappings.
- **Why**: execve and the ELF loader are a self-contained path (6.1's `fs/exec.c` and `binfmt_elf.c`) changed often on their own, and the process map could no longer be read in one Read.
- **Tradeoffs**: Execution spans task teardown in kernel/process, host spawn in kernel/boot and timer clearing in kernel/time, alongside credentials, namespaces, memory, signals and file-related modules; tracing implementation and vDSO construction belong to kernel/ptrace and kernel/vdso.
- **Watch out**: A step added to the point of no return without updating its ordered list in the map.
- **Source**: b320f8e; 8a17e4a9; d053be24; 60790c14; 39e55b4b
