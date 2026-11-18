// Computes fib(50) and prints it in decimal. Covers bl/ret, loops and conditional branches,
// stp/ldp on the stack and pre-indexed strb, udiv/msub.
    .section .rodata
prefix:
    .ascii  "fib(50) = "
    .equ    prefix_len, . - prefix

    .text
    .globl  _start
_start:
    mov     x0, #50
    bl      fib
    mov     x19, x0

    mov     x0, #1
    adrp    x1, prefix
    add     x1, x1, :lo12:prefix
    mov     x2, #prefix_len
    mov     x8, #64                 // __NR_write
    svc     #0

    mov     x0, x19
    bl      print_u64

    mov     x0, #0
    mov     x8, #94                 // __NR_exit_group
    svc     #0

// x0 = n → x0 = fib(n)
fib:
    mov     x1, #0
    mov     x2, #1
    cbz     x0, 2f
1:  add     x3, x1, x2
    mov     x1, x2
    mov     x2, x3
    subs    x0, x0, #1
    b.ne    1b
2:  mov     x0, x1
    ret

// Print x0 in decimal, followed by a newline
print_u64:
    stp     x29, x30, [sp, #-48]!
    mov     x29, sp
    add     x1, sp, #47             // write backwards from the end of the buffer
    mov     w2, #10                 // '\n'
    strb    w2, [x1]
    mov     x3, #10
1:  udiv    x4, x0, x3
    msub    x5, x4, x3, x0          // x5 = x0 % 10
    add     w5, w5, #48             // '0'
    strb    w5, [x1, #-1]!
    mov     x0, x4
    cbnz    x0, 1b

    mov     x0, #1
    add     x2, sp, #48
    sub     x2, x2, x1              // length
    mov     x8, #64                 // __NR_write
    svc     #0
    ldp     x29, x30, [sp], #48
    ret
