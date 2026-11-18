# M9 之前：/proc/<pid>/exe 的跟踪权限（2026-10-31）

[挂载命名空间](118-mount-namespaces.md)那一轮记下 `/proc/<pid>/ns/*`、`root`、`cwd` 跟随时不检查能不能看那个进程，[用户命名空间](119-user-namespaces.md)那一轮补上了这三种，`exe` 一直没查：任何进程都能读别的用户的进程、不可 dump 的进程的程序路径，也能经由它打开那个程序。Linux 的 `proc_pid_readlink`、`proc_pid_get_link` 对这几种链接都先 `proc_fd_access_allowed`（`PTRACE_MODE_READ_FSCREDS`），看不了是 EACCES。

## 做法

- `Pseudo` 读 `exe` 链接时与 `root`、`cwd` 一样先 `may_read`（自己总可以；别的进程要能以 `PTRACE_MODE_READ_FSCREDS` 跟踪它：同一个用户、可以 dump，或者有 CAP_SYS_PTRACE）。tuxport 的 `exe` 按读出来的文字跟随，跟随也经过这里，所以 open、stat、execve 经由它都一样 EACCES；lstat 链接本身不受影响。

## 测试

- 差分测试 `namespaces` 组：别的用户、不可 dump 的进程、新建用户命名空间之后读别的进程的 `exe`（以及 `cwd`、`root`），readlink、open、stat 是 EACCES，lstat 和读自己的照常。

## 推迟项

- `exe` 仍按读出来的文字跟随（[已删除的后缀](97-deleted-suffix.md)的推迟项）：程序文件改了名、被删除之后，经由它打开的不是原来的那个文件。→ 已补上（M9 之前：/proc/<pid>/exe 指向程序文件本身）
