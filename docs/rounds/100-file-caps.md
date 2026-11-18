# M9 之前：文件能力（2026-10-29）

[M6](08-m6.md) 起的推迟项"文件能力和环境能力仍没有"，设计文档 §11.1 也写着 v1 不支持。程序文件的能力存在扩展属性 `security.capability` 里，比如 iputils-ping 的安装脚本用 setcap 给 ping 加 `cap_net_raw`，原来 tuxport 把它当普通的扩展属性存下来，execve 时不看。App 默认以 root 登录，root 本来就有全部能力；普通用户才用得上文件能力，比如 Debian 里 su 成别的用户、sshd 登录的用户，以及以后的"以指定用户登录"。

做的时候发现同一条路径上还缺几样：扩展属性完全不检查权限（任何用户都能设 `security.*`、`trusted.*`，有了文件能力就等于谁都能给自己加能力）；写文件时从来不去掉 setuid 位；securebits 只记录不起作用。这一轮一起按 Linux 6.1 做了。

## 做法

- **execve 的能力变换**（`core/kernel/src/cred.rs` 的 `Cred::exec`，Linux 的 `cap_bprm_creds_from_file`）：
  - 允许集是 (边界集 & 文件的允许集) | (可继承集 & 文件的可继承集) | 环境能力。有效用户 ID 或真实用户 ID 是 0（没有 `SECBIT_NOROOT`）时，文件的两个集合当作全 1；有效用户 ID 是 0 时有效位也置上。setuid root 的程序带着文件能力、由非 root 执行时不按 root 算，只有文件给的。
  - 有效集在文件的有效位置上时等于允许集，否则等于环境能力。文件能力或者 setuid、setgid 清掉环境能力。
  - 文件要求有效位、允许集却得不到全部时（边界集里去掉了），execve 返回 EPERM。
  - `PR_SET_NO_NEW_PRIVS` 之后，或者被建立跟踪时没有 `CAP_SYS_PTRACE` 的跟踪者跟踪着时，得不到原来没有的能力：`Trace` 新增 `privileged`，ATTACH、SEIZE、TRACEME 时记下跟踪者有没有这个能力，相当于 Linux 的 `ptracer_cred`。
  - 特权执行（setuid、setgid，或者非 root 得到了环境能力以外的能力）设置 `AT_SECURE`（原来只看用户 ID、组 ID 变没变）。
  - 与 Linux 的 `commit_creds` 一样，换了有效 ID 或者得到了新的能力时不能 core dump，`PR_SET_PDEATHSIG` 也清掉（原来 setuid 执行时也不清）。
- **程序文件的能力**（`FileCaps`）：
  - execve 时 v1、v2、v3 都认，长度、版本不对时 execve 失败（EINVAL）。没有用户命名空间，v3 只有 rootid 为 0 的起作用。
  - setxattr 只接受 v2、v3：先检查格式（EINVAL），再要求 `CAP_SETFCAP`（EPERM）。getxattr 把 rootid 为 0 的 v3 按 v2 返回（`cap_inode_getsecurity`）。删除也要 `CAP_SETFCAP`。
- **环境能力**：
  - `PR_CAP_AMBIENT` 的四个操作原来是空的，现在提升要求能力同时在允许集和可继承集里、没有 `SECBIT_NO_CAP_AMBIENT_RAISE`。
  - capset 去掉允许集、可继承集里的能力时环境能力跟着去掉；用户 ID 全部变成非 0 时清空（`KEEP_CAPS` 也一样）。
  - `/proc/<pid>/status` 的 `CapAmb` 报告真实的值（原来总是 0）。
- **securebits**：
  - `PR_SET_SECUREBITS` 按 `cap_task_prctl` 检查：要有 `CAP_SETPCAP`，锁住的位不能改，锁不能解开，不认识的位不行。锁住 `KEEP_CAPS` 之后 `PR_SET_KEEPCAPS` 是 EPERM。
  - `NOROOT`、`NO_SETUID_FIXUP`、`NO_CAP_AMBIENT_RAISE` 起作用。
- **扩展属性的权限**（`core/kernel/src/syscall/xattr.rs`，Linux 的 `xattr_permission` 和 commoncap 的钩子）：
  - `trusted.*` 要 `CAP_SYS_ADMIN`：没有时写是 EPERM，读是 ENODATA，列出时看不到。
  - `user.*` 只能在普通文件和目录上，粘滞位目录上只有属主能写，读写要有文件本身的读写权限。
  - 写 `security.capability` 以外的 `security.*` 要 `CAP_SYS_ADMIN`。
- **写、截短、chown 时去掉特权**（`Store::remove_privs`，Linux 的 `file_remove_privs`）：
  - write、pwrite 等写进了数据，以及 copy_file_range、sendfile 写进普通文件、truncate、ftruncate、`O_TRUNC`、fallocate 时：没有 `CAP_FSETID` 的进程去掉 setuid 位和带着组执行位时的 setgid 位，不论谁都去掉文件能力。
  - chown 原来就去掉 setuid、setgid，现在不是目录的也去掉文件能力（`chown_common` 的 `ATTR_KILL_PRIV`）。
  - 写文件很频繁，不能每次都查数据库：元数据缓存里放着有文件能力的 inode 的整个集合，第一次用时查一次，随提交计数一起作废，别的进程设置的也看得到。没有可去掉的时候不碰数据库。

## 测试

- 单元测试：`cred` 测文件能力的各种组合、环境能力、securebits 的锁和 `security.capability` 的格式；`store` 测写和 chown 去掉特权。
- 差分测试 compat 组新增 `showcaps`、`fcaps` 两个 C 程序：各种执行方式得到的能力、扩展属性的权限、去掉特权、securebits 和环境能力的 prctl，与原生逐行相同。docker 里的 root 没有 `CAP_SYS_ADMIN`，`trusted.*`、`security.*` 只比较 nobody 的。
- 第一版漏了 `_GNU_SOURCE`，两边都编不过、输出一样，差分测试照样"通过"：新用例要看一眼输出。
- 写路径的开销在噪声范围内。

## 推迟项

- 没有用户命名空间：rootid 不是 0 的 v3 文件能力不起作用，setcap 也只能在初始命名空间里用。 → 用户命名空间补上了（M9 之前：用户命名空间）；v3 的 rootid 仍只认 0，命名空间里设置的文件能力也不转成 v3
- 跟踪者只记了建立跟踪时有没有 `CAP_SYS_PTRACE`；Linux 记下整份凭据。和别的进程共用文件系统上下文（`CLONE_FS`，Linux 的 `LSM_UNSAFE_SHARE`）时也照样得到特权。
- `system.*`（POSIX ACL）仍按普通的扩展属性存，不检查格式和属主，也不起访问控制的作用。没有 immutable、append-only 属性，`xattr_permission` 里对它们的检查不适用。
- 不记审计日志（`audit_log_bprm_fcaps`）。
