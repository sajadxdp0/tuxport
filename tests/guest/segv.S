// Reads an unmapped address: the guest should terminate with SIGSEGV.
    .text
    .globl  _start
_start:
    mov     x0, #0x1000
    ldr     x1, [x0]
    mov     x0, #0
    mov     x8, #93
    svc     #0
