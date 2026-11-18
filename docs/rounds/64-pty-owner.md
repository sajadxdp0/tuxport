# M9 之前：终端的属主和权限（2026-10-28）

[负载普查](62-workload-survey.md)的问题 2：`ssh -t` 和交互式登录被 sshd 关掉，日志是 `chown(/dev/pts/1, 0, 5) failed: Permission denied`，经由 `ssh -t` 起服务端的 mosh 也就用不了。sshd 的特权进程分配终端之后，把它交给登录的用户和 tty 组（`pty_setowner`：属主或属组不对就 chown，权限不对就 chmod）。

## 原因

- `/dev/pts/<n>` 的属组是打开主设备的进程的 fsgid。tuxport 的 mountinfo 写着 devpts 以 `gid=5,mode=620` 挂载，Linux 按这两个选项把新终端的属组设成 tty（gid 5），所以 root 登录时 sshd 本来不必 chown，在 tuxport 上却要。
- 合成文件系统里的节点一律不能 chown、chmod，返回 EACCES。sshd 遇到 chown 失败，除了只读文件系统，都当作致命错误。

## 做法

- 终端记下属主、属组和权限位（`Tty::owner`、`Tty::mode`）：新终端属于打开主设备的进程的 fsuid 和 tty 组，权限 0620；控制台属于 root 和 tty 组。
- chmod、chown、fchmodat、fchownat 作用于 `/dev/pts/<n>`，fchmod、fchown 作用于从设备的 fd。权限检查与普通文件相同（属主、`CAP_CHOWN`、`CAP_FOWNER`），结果记在终端上，与 Linux 的 devpts 一样到终端关掉为止。别的合成节点仍是 EACCES。
- stat 路径和 fstat 从设备的 fd 都报告这些值：fstat 原来总是 root、0620。

## 测试

- 差分测试 `compat` 新增终端的属主和权限：新终端的属主和属组，root 和普通用户按路径、按 fd 的 chown、chmod。
- 普查的 `sshd`、`sshtools`：root 和普通用户的 `ssh -tt`、mosh 都能用。sshd 的 `-E` 是追加的，`sshtools.sh` 启动之前先删旧日志，免得上次的失败留在里面。
- LTP 的 exit_group01 这次以 SIGABRT 失败，日志在重跑时被覆盖了，之后反复单独跑都通过，原因没查明，记进[偶发项](../milestones.md#known-flaky-tests)。

## 推迟项

- 终端的访问时间、修改时间仍是读的那一刻（w、who 的空闲时间看它），utimensat 仍是 EACCES。→ 已修好（M9 之前：终端的访问时间和修改时间）
- 经由 `/dev/tty` 打开的 fd 上 fchmod、fchown 改的是它对应的 `/dev/pts/<n>`；Linux 上改的是 `/dev/tty` 这个节点本身。
