# M9 之前：用户命名空间（2026-10-30）

让 bubblewrap 能用的第四轮。bwrap 带着 `--unshare-user` 在新的用户命名空间里做挂载、写自己的 uid_map，Codex 每条命令都这样跑。

## 做法

- **凭据**（`core/kernel/src/cred.rs`）：新的 `UserNs`（编号、上一层、层数、建立者的有效用户 ID、uid_map、gid_map、setgroups 的状态），凭据里记着所在的命名空间。凭据里的 ID 照旧是内核 ID（开机时的命名空间里的），`IdMap` 换算：`down` 是 `make_kuid`，`up` 是 `from_kuid`。开机时的命名空间映射全部 ID；每次 `Cred::root()` 新建一份，按编号比较。
- **能力分三种**，照 Linux 每处用的是哪种改过来（原来 74 处都看自己的有效集）：
  - `capable` 是 Linux 的 `capable()`：只认开机时的命名空间，别的命名空间里的进程总是没有。SYS_RESOURCE、SYS_NICE、IPC_LOCK、MKNOD、LEASE、trusted 扩展属性的 SYS_ADMIN、SETFCAP 这些 Linux 本来就这样查，不用改。
  - `capable_wrt` 是 `capable_wrt_inode_uidgid`：自己的命名空间里有，并且文件的属主、属组在里面都有映射。DAC 的越权（DAC_OVERRIDE、DAC_READ_SEARCH）、属主才能做的事（FOWNER，含粘滞位）、chown（CHOWN）、新建和 chmod 时的 setgid 位（FSETID）用它。不持 BKL 的 stat 只给开机时的命名空间里的进程用（别处越权与否要看文件）。
  - `ns_capable` 是 `cap_capable`：在目标命名空间里有，或者自己在它的祖先命名空间里、有效用户 ID 是那条线上建立者。setuid 系列、setgroups、capset、securebits、`PR_CAPBSET_DROP`、chroot、`CLONE_NEWNS` 看自己的命名空间；mount、umount、pivot_root、remount、`MS_MOVE` 看挂载命名空间的属主（挂载命名空间记下建立它时所在的用户命名空间，Linux 的 `may_mount`）；ptrace 的访问检查、读别人的 robust list 看目标的（CAP_SYS_PTRACE）；kill 看目标的（CAP_KILL）。
  - root 的判断改成"自己的命名空间里的用户 ID 0"（`make_kuid(ns, 0)`，可能没有映射）：setuid 系列之后调整能力（`cap_emulate_setxuid`、fsuid 的）、execve 时把文件能力当作全 1（`handle_privileged_root`）、`AT_SECURE`、access(2) 用的能力。
- **新建**（`core/kernel/src/syscall/ns.rs`）：`clone`、`unshare` 的 `CLONE_NEWUSER` 不要特权；最深到第 33 层（再深是 ENOSPC，Linux 6.1；老内核是 EUSERS），chroot 了（根目录不是挂载命名空间的根）是 EPERM，建立者的有效用户 ID、组 ID 在原来的命名空间里要有映射。新的命名空间里拥有全部能力、securebits 清零（`set_cred_user_ns`），ID 不变、还没有映射，对外显示成 65534。`CLONE_NEWUSER` 和 `CLONE_THREAD`、`CLONE_FS` 不能同用；unshare 连带 `CLONE_THREAD | CLONE_FS`，多线程的进程是 EINVAL。和 `CLONE_NEWNS` 一起时先建用户命名空间，新的挂载命名空间属于它、按它的能力检查（clone 里新进程的凭据也先算好，unshare 所有检查都通过了才改）。
- **`/proc/<pid>/uid_map`、`gid_map`、`projid_map`、`setgroups`**（task 子目录里也有）：读按 `uid_m_show`，每段三列各占 10 个字符，第二列按读者的命名空间换算，读者就在这个命名空间里时按上一层的；projid_map 只有开机时的命名空间有。写 uid_map、gid_map 按 `map_write`：只能写一次，写的进程要在那个命名空间里有 CAP_SYS_ADMIN（EPERM），每行三个数、个数不为 0、不溢出、不重叠、最多 340 段（EINVAL）；然后是 `new_idmap_permitted`：建立者把自己的有效 ID 映射成一段一个（gid_map 要先把 setgroups 写成 deny），或者在上一层命名空间里有 CAP_SETUID、CAP_SETGID，否则 EPERM；第二列按上一层的映射换成内核 ID（跨段时拆开），映射不到是 EPERM。setgroups 写 allow 或 deny（后面只能有空白）：禁止了不能再允许，gid_map 写过之后不能禁止。gid_map 没写或者 setgroups 禁止了时 setgroups(2) 是 EPERM。
- **对外的 ID**：getuid 这些、getresuid、getgroups 按自己的命名空间换算；setuid 系列、setgroups、chown 的参数映射不到是 EINVAL，setfsuid、setfsgid 静默忽略；newfstatat、statx、fstat 的属主属组在出口统一换算；`/proc/<pid>/status` 的 Uid、Gid、Groups 按读者的命名空间。execve 的 setuid、setgid 位要文件的属主、属组在自己的命名空间里有映射才生效（`bprm_fill_uid`）。
- **setns 到用户命名空间**（`userns_install`）：不能是自己的（EINVAL），多线程、fs 共用时 EINVAL，要在目标里有 CAP_SYS_ADMIN（EPERM），进去之后有全部能力。`/proc/<pid>/ns/user` 是那个进程的。
- **顺带照 Linux 补上的**：
  - ptrace 的访问检查加上 commoncap 那一步（`cap_ptrace_access_check`）：两者在同一个用户命名空间里、目标的允许集不超过自己的，或者在目标那边有 CAP_SYS_PTRACE。`/proc/<pid>/ns/*`、`root`、`cwd` 跟随和读链接时按它检查（`PTRACE_MODE_READ_FSCREDS`，不许是 EACCES），所以 `Pseudo::follow` 能返回错误了。
  - setuid 系列改了有效、文件系统的用户或组，或者得到了新的能力时，进程不再可以 dump、父进程退出时不再给它发信号（`commit_creds`，原来只有 execve 这样做）；不可 dump 的进程的 `/proc/<pid>` 下的文件属于它的命名空间里的 root（`task_dump_owner`）。execve 普通程序时恢复成可以 dump（`begin_new_exec`），程序文件对调用者不可读时除外（`would_dump`），原来从不恢复。
  - `/proc/sys/user/max_*_namespaces`（threads-max 的一半，只读，不实际限制）。

## 测试

- 单元测试 `user_namespaces`：映射的解析和换算、三种能力检查、换用户和 execve 之后的能力。
- 差分测试 `namespaces` 组加上 root 建的用户命名空间：映射和 setgroups 的读写和各种错误，里面看到的 ID 和能力，从外面读它的映射和 ID，setns，unshare 的错误和嵌套层数，两个执行层下都与原生相同。
- 普通用户自己建的命名空间差分测试比不了：开发用的虚拟机和 GitHub 的 runner 都是 Ubuntu 24.04，它的 `kernel.apparmor_restrict_unprivileged_userns` 不让这种命名空间里有能力，关掉它要改宿主的安全设置，没有做。改为集成测试 `user_namespace_of_an_unprivileged_user` 按上游 Linux 的行为检查：nobody 用 `unshare -r -m` 映射成 root 之后，能做和不能做的事。
- LTP 的 sysinfo01 这次失败，查出来与这一轮无关：`/proc/uptime` 四舍五入（Linux 截断到百分之一秒）、sysinfo 截断（Linux 有零头就进一秒），x.996 秒时两者颠倒，照 Linux 改了两处。ioctl_ns06 由失败变成跳过（走到了还没有的 `NS_GET_USERNS`）。

## 推迟项

- pid、网络、IPC、UTS、cgroup、time 命名空间还没有（下一轮起）。 → pid 命名空间已补上（M9 之前：pid 命名空间），UTS、IPC 命名空间也补上了（M9 之前：UTS 和 IPC 命名空间），网络命名空间也补上了（M9 之前：网络命名空间），cgroup 命名空间也补上了（M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱），time 命名空间也补上了（M9 之前：time 命名空间）
- 还按内核 ID 报告、没有换算的：`SO_PEERCRED`、`SCM_CREDENTIALS`、siginfo 的 `si_uid`、SysV IPC 和消息队列的属主、core dump 里的 ID。→ 已补上（M9 之前：用户命名空间里的 ID 换算），连同 waitid 的 si_uid、auxv 的 `AT_UID` 这几项、`/proc/net/tcp` 等的 uid 列（这里的消息队列指 SysV 的）
- 写映射时按写的进程的凭据检查，Linux 有两条按打开文件的凭据（`file_ns_capable`）；projid_map 不能写。
- 复制到权限更小的用户命名空间里的挂载不上锁（Linux 的 `MNT_LOCKED`）：里面可以单独卸下、去掉 nosuid 这些；在用户命名空间里挂 procfs、sysfs 不检查 `mount_too_revealing`。
- `max_*_namespaces` 只读、不实际限制，没有 ucounts。 → 可以写了（M9 之前：网络命名空间）；仍不实际限制，没有 ucounts
- v3 的文件能力仍只认 rootid 0，命名空间里设置文件能力不转成 v3（[文件能力](100-file-caps.md)的推迟项）；ptrace 建立跟踪时记的 CAP_SYS_PTRACE 仍按开机时的命名空间。
- 命名空间文件不让用户命名空间活着：里面的进程都退出之后，对打开着的 `/proc/<pid>/ns/user` setns 是 EINVAL。→ 已补上（M9 之前：命名空间文件留住命名空间）
- 不持 BKL 的 stat 只给开机时的用户命名空间里的进程用。
- 普通用户自己建的用户命名空间没有和原生比对过（见上），只按上游的行为写了集成测试。
