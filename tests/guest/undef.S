// An undefined instruction: the guest should terminate with SIGILL.
    .text
    .globl  _start
_start:
    udf     #0
