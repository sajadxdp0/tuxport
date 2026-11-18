# M9 之前：PR_SET_CHILD_SUBREAPER（2026-10-31）

[pid 命名空间](120-pid-namespaces.md)的推迟项。`PR_SET_CHILD_SUBREAPER` 让一个进程收养后代里的孤儿：子进程退出时，它的子进程不再交给 1 号进程，而是交给祖先里最近的 subreaper。tini、dumb-init、catatonit、s6 这类进程管理程序和 `systemd --user` 靠它回收自己起的服务留下的进程。原来 prctl 对这两个选项返回 EINVAL，这些程序要么报错退出，要么收养不到孤儿；LTP 的 prctl03 因此跳过。

## 做法

- 线程组上记一个 `child_subreaper`（`core/kernel/src/process.rs`）。`PR_SET_CHILD_SUBREAPER` 的参数非 0 就设上（与 Linux 一样不看别的参数），`PR_GET_CHILD_SUBREAPER` 往参数指的 int 里写 0 或 1（写不进去是 EFAULT）。fork 不继承，execve 保留。
- 进程结束时（`finish_group` 的第 2 步）先找 subreaper：从父进程往上，找第一个设了它、还有活着的线程的进程（Linux 的 `find_new_reaper`）。只在同一层 pid 命名空间里找：与 Linux 一样比 pid 的层数，往上走到外面那层就停，外面的 subreaper 不收养命名空间里的孤儿。找不到才照旧交给所在的 pid 命名空间的 1 号进程或内置的 init。命名空间的 1 号进程自己退出时（里面的进程都被结束）不找。
- 交出去的过程不变：孤儿记到新的父进程下，已经是僵尸的再通知它一次（`reparent_leader`），父进程退出时的 `PR_SET_PDEATHSIG`、孤儿进程组的判断都照旧。

## 测试

- 差分测试 `compat` 组新增 `subreaper.c`：两个选项的读写、fork 和 execve 之后、孤儿和僵尸过继给最近的 subreaper 并由它回收；`namespaces` 组新增：外面的 subreaper 不收养新 pid 命名空间里的孤儿。两个执行层下都与原生相同。
- 用例自己的两处问题：fork 之前 stdout 缓冲着的内容被子进程又输出一遍（改成行缓冲）；最后用 `waitpid(-1, WNOHANG)` 查有没有剩下的子进程，原生一侧偶尔得到 0，去掉了。
- LTP 的 prctl03 由跳过变成通过。

## 推迟项

无。
