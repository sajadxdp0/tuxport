# M9 之前：关掉主设备时挂断从设备（2026-10-29）

[AI 编程 CLI](103-ai-cli-survey.md)那一轮找到的：tmux 里开着 opencode，`tmux kill-server` 之后它在原生上退出，在 tuxport 里留下来，按 25 毫秒一次的周期一直在画界面。几个 CLI 轮流开的时候前一个占着内存，后一个就被 OOM killer 结束了。

原因是关掉 PTY 的主设备时 tuxport 只把计数减一、叫醒等待者。Linux 关掉最后一个主设备时（`pty_close` → `tty_vhangup`）挂断从设备，`__tty_hangup` 做这几件事：

- 会话组长收到 SIGHUP 和 SIGCONT（`tty_signal_session_leader`），会话里别的进程收不到；组长身上记下这时的前台进程组（`tty_old_pgrp`），组长退出时已经没有控制终端，`disassociate_ctty` 改为给这个进程组发 SIGHUP 和 SIGCONT；
- 会话里以它为控制终端的进程都失去控制终端，之后打开 `/dev/tty` 是 ENXIO；
- 打开着的从设备换成 `hung_up_tty_fops`：读返回 0（还没读的输入也不要了），写是 EIO，ioctl 是 EIO（TIOCSPGRP 是 ENOTTY），poll 总是报告 IN、OUT、ERR、HUP；
- `/dev/pts/<n>` 删掉，编号还回去（`devpts_pty_kill`），从设备还开着也一样。

tuxport 原来：组长收不到信号，所以捕获 SIGHUP 的 opencode 不知道终端没了；从设备的读返回 EIO，而且队列里还有数据时先把数据读出来；ioctl 照常成功，`isatty` 仍为真；poll 只报告 IN、HUP；`/dev/pts/<n>` 还列着，打开是 EIO（Linux 是 ENOENT）。App 关掉控制台时会给会话组长发 SIGHUP 和 SIGCONT，但从设备不挂断，会话也不失去控制终端。

## 做法

- `core/kernel/src/syscall/tty.rs` 的 `hang_up` 按上面的几步挂断，终端记下已经挂断（`Tty::hung_up`），从设备的读、写、ioctl、poll 照 `hung_up_tty_fops` 处理。App 关掉控制台（`terminal::close`）也调用它，原来只发两个信号。
- 组长退出时（`process::release_ctty`）控制终端已经挂断了的，给挂断时记下的前台进程组发 SIGHUP 和 SIGCONT；重新得到控制终端时清掉记下的进程组（Linux 的 `__proc_set_tty`）。tmux 用 `sh -c` 起窗口里的命令：Alpine 的 busybox sh 直接 exec，命令自己就是会话组长；Debian、Ubuntu 的 dash 不 exec，会话组长是 sh，命令和它在同一个进程组里，靠的就是这一步。只做了挂断的那几步时，Debian 上的 codex、gemini 在 `kill-server` 之后仍然留下来，在 Alpine 上让 tmux 用 dash 起 `python3 -c 'time.sleep(30)'` 也一样；原生的都退出了。
- 关闭打开文件是在 `PtyEnd` 的 Drop 里，拿不到内核。最后一个主设备关掉时，它把终端记在线程局部的表里，持有 BKL 的线程放开 BKL 之前（`Bkl` 的 Drop）把表里的都挂断，再叫醒等待者。这一步出了内核 bug 时只报告。
- **tuxport-run 输入结束时的 EOF 丢了**：写测试时压出来的，是[上一轮](102-snmp-pty-eof.md)的问题。负载高时，管道给 `sh -i` 的输入有大约四分之一会卡住：ash 读完一行就切回规范模式去执行命令，桥接线程看到输入读空了，就在这时送出 VEOF。规范模式下它成了行尾的 EOF 标记，ash 回到提示符时行编辑切成非规范模式，排着的行变成普通的字节，这个标记就没了。Linux 上它会变成一个 NUL 字节，ash 照样不退出，util-linux 的 `script` 有同样的竞争。桥接线程现在记着 VEOF 是不是在规范模式下送的；如果是，而且程序没读到它终端就切到了非规范模式、输入又读空了，就再送一次，这次是 ^D 字符。

## 测试

- 差分测试 compat 组新增一个 Python 用例：不等读走就关掉主设备之后，会话组长、组长进程组里的和另起进程组的进程收到的信号，挂断的从设备的 poll、读、写、终端 ioctl，`/dev/tty` 和 `/dev/pts` 里的节点，与原生逐行相同。
- 在 tmux 里开 opencode、gemini、codex（Alpine 上直接起和用 dash 起，以及 Debian 上），`kill-server` 之后它们都退出了，与原生相同。
- 管道给 `tuxport-run --pty` 的 `sh -i` 和 `cat` 输入、8 个一批并发：改之前 Tier 1 下约三分之一卡住，改之后两层都正常退出。

## 推迟项

- `vhangup(2)`、`TIOCVHANGUP` 没有实现：程序自己不能挂断终端。→ 已补上（M9 之前：vhangup）
- 规范模式切到非规范模式时，排着的 EOF 标记丢掉，Linux 上变成 NUL 字节；以 VEOF 结束的不完整的行也不记 VEOF 的位置。只有切换模式的那一刻还没读走的输入受影响。
- 已经挂断的从设备在 `/proc/<pid>/fd` 里的路径仍是 `/dev/pts/<n>`；Linux 上节点删掉了，后面带 ` (deleted)`。编号给了新的终端之后，两个终端的设备号相同，Linux 上也是这样。
- tuxport-run 的桥接线程最多补送一次：程序在规范模式下读到了 EOF 之后接着运行，后来又切到非规范模式时，这一次补送它也会收到。
