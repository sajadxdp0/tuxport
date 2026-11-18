// Prints argv[1..] joined with spaces, to check the initial stack layout (argc, argv, strings).
    .section .rodata
space:
    .ascii  " "
newline:
    .ascii  "\n"

    .text
    .globl  _start
_start:
    ldr     x19, [sp]               // argc
    add     x20, sp, #8             // argv
    mov     x21, #1                 // i
1:  cmp     x21, x19
    b.ge    4f
    ldr     x1, [x20, x21, lsl #3]  // argv[i]
    mov     x2, #0
2:  ldrb    w3, [x1, x2]            // strlen
    cbz     w3, 3f
    add     x2, x2, #1
    b       2b
3:  mov     x0, #1
    mov     x8, #64                 // __NR_write
    svc     #0
    add     x21, x21, #1
    cmp     x21, x19
    b.ge    4f
    mov     x0, #1
    adrp    x1, space
    add     x1, x1, :lo12:space
    mov     x2, #1
    mov     x8, #64
    svc     #0
    b       1b

4:  mov     x0, #1
    adrp    x1, newline
    add     x1, x1, :lo12:newline
    mov     x2, #1
    mov     x8, #64
    svc     #0
    mov     x0, #0
    mov     x8, #93                 // __NR_exit
    svc     #0
