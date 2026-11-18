# M9 之前：终端的访问时间和修改时间（2026-10-28）

[终端的属主和权限](64-pty-owner.md)那一轮的推迟项：`/dev/pts/<n>` 的访问时间、修改时间、状态改变时间都报告 stat 的那一刻，utimensat 是 EACCES。w、who 按终端的访问时间算空闲时间，在 tuxport 上永远是 0；util-linux 的 write 在用户有多个终端时挑访问时间最近的那个。

## Linux 的做法

devpts 的 inode 和普通文件一样有三个时间。6.1 的 `tty_update_time`：从设备读到数据时改访问时间，写出数据时改修改时间，只改秒，而且只在和原来相差 8 秒以上时才改，注释说精确的时刻会泄露别人敲键盘的节奏。chmod、chown、utimensat 改状态改变时间，utimensat 可以把访问、修改时间设成任意值。

## 做法

终端记下三个时间，打开主设备、建这个终端时取当前时间。从设备的读写按上面的规则更新；stat、fstat 报告它们；utimensat 和 chmod、chown 一样作用到终端上（`UTIME_NOW` 取当前时间，权限规则与仓库里的文件相同），并把状态改变时间改成现在；chmod、chown 也改状态改变时间。socket、匿名管道的 fd 上的 futimens 也成功了（和[第 77 轮](77-dead-dir-socket-fchmod.md)的 fchmod 一样，不记下来）。

## 测试

- 终端的单元测试新增 `reads_and_writes_touch_the_times_every_8_seconds`。
- 差分测试 compat 组加了终端上的 utimensat、chmod、读写之后的三个时间，两层都与原生相同。

## 推迟项

- 经由 `/dev/tty` 打开的 fd 上的读写也改 `/dev/pts/<n>` 的时间；Linux 6.1 上改的是 `/dev/tty` 这个节点自己的（`file_inode`）。主设备（`/dev/ptmx`）的读写不改任何时间，fstat 报告的时间是 0。
- 没有在 sshd 的登录会话里实测 w、who 的空闲时间。→ 已实测（M9 之前：没有实现的 netlink 协议建不了）：Alpine 的 musl 不写 utmp，w、who 什么也不列；Debian 的 sshd 会话里 w 的空闲时间与原生相同，实测时先修好了 Debian 上 PAM 登录不了的问题
