# M9 之前：删除的目录里不能新建，socket 可以 fchmod（2026-10-28）

[翻译表那一轮](72-table-tombstones.md)重跑 Node.js 的测试时新找到的两个问题。

## 做法

- **已经删除的目录里还能新建**：进程 chdir 进一个目录、把它 rmdir 之后，用相对路径 mkdir 成功了，Linux 上是 ENOENT（`may_create` 看到 `IS_DEADDIR`）。test-fs-mkdir 因此失败。
  - 改动：仓库在目录里新建条目（create、mkdir、mknod、symlink、link）时，目录的链接数已经是 0 就返回 ENOENT；改名到这样的目录里也是。删除时目录必须是空的，所以里面不会有已有的条目。
- **对 socket 的 fd 做 fchmod 返回 EACCES**：libuv 的 `uv_pipe_chmod` 先 fchmod socket 的 fd，Linux 上改的是 sockfs 里它自己的 inode，会成功；只有 EINVAL、EOPNOTSUPP 才退回按路径 chmod。tuxport 的 fchmod 对仓库之外的 fd 返回 EBADF，musl 的 fchmod 遇到 EBADF 时退回 `chmod("/proc/self/fd/N")`（给 `O_PATH` 打开的 fd 用的），走到合成的节点上成了 EACCES。Node 的 `listen({ path, readableAll, writableAll })` 因此失败（test-net-server-listen-path、test-cluster-listen-pipe-readable-writable）。
  - 改动：socket 和匿名管道的 fd 可以 fchmod、fchown，当作调用者自己的（Linux 上属主是创建它的进程），按原来的规则检查权限，改了不记下来。

## 测试

- 仓库的单元测试新增 `deleted_directories_take_no_new_entries`。
- 差分测试 compat 组加了删除掉的当前目录里新建的错误码、socket 和管道的 fd 上的 fchmod，两层都与原生相同。
- 上面三个 Node 测试都通过；test/parallel 整套重跑，比原生多出的失败都是被 OOM killer 结束的（[第二轮普查](70-survey-suites.md)的问题 5）。

## 推迟项

- socket、匿名管道的 fstat 报告的属主（0）和权限是合成的，fchmod、fchown 改了也看不到；Linux 上是创建它的进程的 fsuid 和改过的权限。
- 经由 `/proc/<pid>/fd/<n>` 对 socket、管道 chmod 仍然是 EACCES（Linux 上跟着魔法链接改的是同一个 inode）。
