# M9 之前：UTS 和 IPC 命名空间（2026-10-30）

让 bubblewrap 能用的第六轮。Codex 的命令都带 `--unshare-ipc`，`--unshare-uts`、`--hostname` 也常用；原来 `CLONE_NEWUTS`、`CLONE_NEWIPC` 是 EPERM。网络命名空间要另外做一套隔离的回环，下一轮单独做。

## 做法

- **放在哪**（`core/kernel/src/nsproxy.rs`）：与 Linux 的 `struct nsproxy` 一样按线程记在 `Task` 上（unshare 这两种不连带 `CLONE_THREAD`，多线程的进程里 unshare 只改调用的线程），clone 出来的线程、进程沿用父线程的。开机时的一份放在内核里，开机时的进程共用。`/proc/<pid>/ns/uts`、`ipc` 按线程报告。
- **UTS 命名空间**：主机名、NIS 域名原来在内核里只有一份，现在每个命名空间一份，新建时复制原来的（Linux 的 `copy_utsname`）。uname、sethostname、setdomainname、`/proc/sys/kernel/hostname`、`domainname`、core 文件名的 `%h` 都按所在的命名空间；sethostname 要在命名空间的属主里有 `CAP_SYS_ADMIN`（原来是 `capable()`），所以 root 建的用户命名空间里新建的 UTS 命名空间可以改名字。
- **IPC 命名空间**：System V 的消息队列、信号量、共享内存段，SEM_UNDO 的撤销值，`/proc/sys/kernel/msg*`、`sem`、`shm*` 这些限制，POSIX 消息队列和 `/proc/sys/fs/mqueue/*`，原来都在内核里只有一份，现在每个命名空间一份（`Kernel::ipcs`，按命名空间的编号）。新建的命名空间是空的、限制是默认值（Linux 的 `create_ipc_ns`），id 从头编号。各个系统调用、`/proc/sysvipc/*` 按调用者、读者所在的命名空间。共享内存段的映射记下段所在的命名空间和 id（各个命名空间的 id 会重复），shmdt 和数 `shm_nattch` 都按它，unshare 之后照样能 shmdt 原来的段；进程退出时各个命名空间里它的撤销值都执行；最后一个用某个命名空间的线程退出时丢掉里面的对象（还映射着的段的内存由映射留着，Linux 的 `free_ipc_ns`）。
- **新建和进入**（`core/kernel/src/syscall/proc.rs`、`syscall/ns.rs`）：clone、unshare 的 `CLONE_NEWUTS`、`CLONE_NEWIPC` 要在（可能是新建的）用户命名空间里有 `CAP_SYS_ADMIN`（EPERM），新的命名空间属于它；clone 的 `CLONE_NEWIPC` 和 `CLONE_SYSVSEM` 不能同用（EINVAL，在能力检查之后，Linux 的 `copy_namespaces`）。unshare 的 `CLONE_NEWIPC` 像 `CLONE_SYSVSEM` 一样先执行并丢掉 SEM_UNDO 的撤销值（原来的命名空间里的信号量以后够不着了）。setns 到 UTS、IPC 命名空间（`utsns_install`、`ipcns_install`）要在它的属主和自己的用户命名空间里都有 `CAP_SYS_ADMIN`，换 IPC 命名空间之前也执行撤销值。

## 测试

- 差分测试 `namespaces` 组加上：UTS 命名空间里的主机名、域名和它们的错误、setns；新的 IPC 命名空间里看不到外面的各种 IPC 对象、unshare 时执行撤销值、shmdt 原来的段、里面的限制，setns 之后看到别的进程的对象。两个执行层下都与原生相同。
- 顺带修了上一轮用例里两处先后不定的打印：两个进程同时打印；1 号进程睡得太短，慢的执行层下可能在外面打印之前醒来。
- `scripts/ltp` 给 LTP 的内核配置加上 `CONFIG_UTS_NS`、`CONFIG_IPC_NS`，setns02 新通过。

## 推迟项

- 没有网络命名空间（下一轮）；cgroup、time 命名空间仍没有。 → 网络命名空间已补上（M9 之前：网络命名空间），cgroup 命名空间也补上了（M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱），time 命名空间也补上了（M9 之前：time 命名空间）
- 没有 mqueue 文件系统（原来就没有），POSIX 消息队列只能经由 mq_open 这些系统调用用。→ 已补上（M9 之前：mqueue 文件系统）
- 在用户命名空间里的 root 写 `/proc/sys/kernel` 下的 IPC 限制、主机名仍按文件本身的属主检查（Linux 的 IPC 限制按命名空间的属主，`ipc_set_ownership`）。
- 命名空间文件不让 UTS、IPC 命名空间活着（与[挂载命名空间](118-mount-namespaces.md)的推迟项相同）。→ 已补上（M9 之前：命名空间文件留住命名空间）：IPC 命名空间里没删的对象随它留着
- `max_uts_namespaces`、`max_ipc_namespaces` 只读、不实际限制。 → 可以写了（M9 之前：网络命名空间），仍不实际限制
