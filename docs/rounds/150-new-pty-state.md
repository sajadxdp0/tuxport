# M9 之前：新分配的 PTY（2026-11-01）

[Ubuntu 26.04 的普查](149-ubuntu26-survey.md)的问题 1 和 3，都是新分配的终端的初始状态。

问题 1：GNU screen 的窗口时好时坏（普查里没有运行，单独跑时多半正常）。screen 的主进程分配好终端、fork 出窗口的子进程之后马上读主设备，子进程这时可能还没打开从设备。tuxport 的主设备在"已经解锁、从设备一个都没打开着"时读就返回 EIO、poll 带 POLLHUP，screen 就当窗口已经结束了。Linux 只在从设备打开过、又全部关掉之后（`pty_close` 设的 `TTY_OTHER_CLOSED`）才这样，从来没打开过时照常阻塞（`ptm_unix98_ops` 装好一对终端时从设备的计数就是 1，不算关掉）；原生上确认过：已解锁、没打开过时读是 EAGAIN、没有 POLLHUP，打开再关掉之后才是 EIO 和 POLLHUP，重新打开又恢复。tuxport 的注释写的正是"从设备打开过又全部关闭之后返回 EIO"，实现漏了"打开过"。

问题 3：util-linux 的 `script` 在 stdin 不是终端时不设窗口大小，里面看到的在原生上是 0×0，tuxport 是 24×80：tuxport 新建终端时的窗口大小用的是给控制台的默认值。

## 做法

- `Tty::slave_closed`（Linux 的 `TTY_OTHER_CLOSED`）：最后一个从设备（包括经由 `/dev/tty` 打开的）关掉时设上，打开从设备时清掉。主设备的读按它返回 EIO，poll 按它报告 POLLHUP，不再看从设备的个数和锁。
- 新建的终端窗口大小全是 0（Linux 的 `tty_struct` 是清零分配的）。控制台建好之后照旧设成宿主终端的大小，不知道时 24×80（`terminal::open`）。

## 测试

- 差分测试 `pty` 组：新终端的 `TIOCGWINSZ`；从设备锁着、没打开过、打开着、关掉一部分和全部、再锁上、重新打开时主设备的 poll 和非阻塞读；主设备阻塞地读、子进程晚一点才打开从设备。两个执行层下都与原生逐字相同。原来的实现在再锁上之后也不是 EIO（Linux 是），是变异检查时顺带看出来的。
- GNU screen：Ubuntu 26.04 的仓库里带着 `TUXPORT_LOG=1`（普查的条件）连跑 6 次 `screen -D -m`，窗口里的命令每次都运行了。
