// M0 exit criterion: a hand-written static aarch64 program that uses only write + exit.
    .section .rodata
msg:
    .ascii  "Hello from tuxport!\n"
    .equ    msg_len, . - msg

    .text
    .globl  _start
_start:
    mov     x0, #1                  // fd = stdout
    adrp    x1, msg
    add     x1, x1, :lo12:msg
    mov     x2, #msg_len
    mov     x8, #64                 // __NR_write
    svc     #0

    mov     x0, #0
    mov     x8, #93                 // __NR_exit
    svc     #0
