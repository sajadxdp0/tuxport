// The exit code must reach the host unchanged.
    .text
    .globl  _start
_start:
    mov     x0, #42
    mov     x8, #94                 // __NR_exit_group
    svc     #0
