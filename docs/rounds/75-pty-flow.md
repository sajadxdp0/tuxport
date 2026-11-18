# M9 之前：PTY 的 TCFLSH 和 TCXONC（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 4：PTY 的 TCFLSH 在主设备上方向反了，TCOOFF 不让从设备的写阻塞。CPython 的 test_termios、test_ioctl 因此各卡到 20 分钟的超时。

## 原因

Linux 的 PTY 是一对 tty，从设备的输出就是主设备的输入。

- TCFLSH：在主设备上，TCIFLUSH 清的是从设备写出、主设备还没读的数据；TCOFLUSH 经由 `pty_flush_buffer` 只清还没交给从设备行规程的数据。tuxport 两端一律按从设备的方向清：主设备的 TCOFLUSH 把从设备写出的数据清掉了，test_tcflush_clear_input_or_output 接下来的读永远等不到。
- TCXONC：TCOOFF 停住这一端的输出，这一端的 write 阻塞到 TCOON（`pty_write_room` 为 0，poll 也不报 POLLOUT），已经写出的数据另一端照样能读。tuxport 只把一个 `stopped` 标志置上，从设备的 write 照样立刻成功，主设备反而读不到停住之前就已写出的数据；TCIOFF、TCION 什么也不做。

## 做法

- TCFLSH 在主设备上：TCIFLUSH 清从设备写出的数据；TCOFLUSH 不清任何东西，主设备写进来的数据当场就交给了从设备的行规程，Linux 上那部分也不清。从设备上不变。
- 从设备的输出被停住（^S 或 TCOOFF，Linux 的 `flow.stopped`）时，输出队列不再接收，从设备的写阻塞，也不报 POLLOUT。停住之前写出的数据主设备照样能读；停住期间产生的回显暂存起来，恢复时才给主设备，Linux 也是把它们留在回显缓冲区里。
- TCOOFF 和 ^S 分开记（Linux 的 `flow.tco_stopped`）：TCOOFF 停住的，^Q、IXANY、产生信号都恢复不了，要 TCOON。产生信号时只在设了 IXON 时恢复输出，与 `n_tty_receive_signal_char` 相同（原来无条件恢复）。
- 主设备有自己的停止状态：主设备上 TCOOFF 之后，主设备的写阻塞到 TCOON。
- TCIOFF、TCION 发 STOP、START 字符（禁用的不发）：从设备发给主设备，不受停住的限制，排在暂存的回显之前（`tty_send_xchar`）；主设备发的字符作为从设备的输入，经过 IXON 处理。

## 测试

- 终端的单元测试新增 `stopped_output_blocks_the_slave_but_keeps_what_was_written`。
- 差分测试 compat 组加了 PTY 的检查：两端的 TCFLSH，TCOOFF、^S 停住时的写和 poll、怎样才能恢复，TCIOFF、TCION 发的字符，两层都与原生相同。
- CPython 的 test_termios、test_ioctl、test_pty、test_tty、test_readline、test_getpass 全部通过。

## 推迟项

- 从设备的 TCOFLUSH 和产生信号时（没有 NOFLSH）仍然清掉主设备还没读的全部输出。Linux 上只清还在 flip buffer 里、没交给主设备行规程的部分，通常没有可清的。
- 包模式（TIOCPKT）整个没有实现，停住、恢复、清空时也就没有 `TIOCPKT_STOP`、`TIOCPKT_START`、`TIOCPKT_FLUSHREAD` 这些通知。→ 已补上（M9 之前：PTY 的包模式），GNU screen 离不开它
