// An unimplemented syscall must return -ENOSYS instead of crashing the program
// (design doc §7.5). The exit code is 0 when it returns -38, otherwise 1.
    .text
    .globl  _start
_start:
    mov     x8, #1000               // a syscall number that does not exist
    svc     #0
    cmn     x0, #38
    cset    x0, ne
    mov     x8, #93                 // __NR_exit
    svc     #0
