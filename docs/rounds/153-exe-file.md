# M9 之前：/proc/<pid>/exe 指向程序文件本身（2026-11-01）

[已删除的后缀](97-deleted-suffix.md)和 [exe 的跟踪权限](147-proc-exe-access.md)两轮的推迟项：tuxport 的 `/proc/<pid>/exe` 只记着执行时的路径，跟随时按这段文字重新解析。程序文件改了名、被删除或者被换掉（dpkg、apk 升级时用 rename 换掉正在运行的程序）之后，经由它 stat、open、execve 到的是同一路径上的新文件，或者 ENOENT；读出来的路径也不带 ` (deleted)`（带了的话，按文字跟随就到不了）。Linux 的 exe 是 `mm->exe_file`，跟随时直接跳到那个文件（`proc_exe_link`、`nd_jump_link`），删除之后读出来的 `d_path` 带 ` (deleted)`。

用到它的：Chromium、Electron 这类经由 `/proc/self/exe` 起子进程的程序，升级之后父子进程是两个版本；needrestart 按 exe 的 ` (deleted)` 找出升级后要重启的进程。

## 做法

- `ExeFile`（`core/kernel/src/process.rs`，Linux 的 `mm->exe_file`）：执行时的路径、经由的挂载和仓库里的 inode、最后经过的目录和名字，以及打开着的宿主文件（loader 读程序时打开的那个，同一个程序的进程共用）。execve 和起第一个进程时记下，fork 共用。
- 读链接：路径，inode 的 nlink 是 0 时加 ` (deleted)`，与 fd、maps 一样（`mark_deleted`）。
- 跟随（`Pseudo::follow`）：没删除的直接到 `Node::Store(挂载, inode)`，改了名、被换掉的也是原来那个；删除了的仓库里的 object 也删了，到新的节点 `PseudoNode::OpenExe`，经由记着的宿主文件：
  - stat：`stat_open`，设备号按原来的挂载；
  - open：复制宿主文件，与打开已经删除的 fd 一样；以写方式打开、截短仍在执行的程序是 ETXTBSY，权限先查；
  - execve：`open_exe_file` 打开记着的宿主文件，权限、noexec、nosuid 的检查与按路径执行时相同；新程序的 exe 沿用原来的挂载、inode 和目录项，读出来仍是 `原来的路径 (deleted)`；
  - 不能当挂载点（ENOENT），与经由 fd 到了已经删除的文件一样。
- `follow_dentry` 给出 exe 记着的目录项：经由 `/proc/self/exe` 执行时，新程序的路径按它算，不再是 `/proc/self/exe`。`Vfs::real_path` 只剩单元测试用，execve 改成 `walk` 加 `found_path`，同时拿到目录项。
- 内置的 init 没有 exe（None），读链接仍是 ENOENT。开发用的宿主文件（不在仓库里）没有挂载和 inode，仍按路径跟随。

## 测试

- 差分测试 `compat2` 组的 `exelink.c`：程序把自己复制一份再执行，然后删掉、用 rename 换掉、只改名自己的程序文件，看读链接、经由它 stat、open、`execl("/proc/self/exe")` 和 maps，以及别的进程经由 `/proc/<pid>/exe` 看到的，Tier 1 下与原生逐字相同。只改名时读出来的路径 Linux 跟着变、tuxport 不变，不比。
- 写用例时原生一侧卡住过一次：等着的子进程继承了另一根管道的写端，读不到 EOF，管道改成 `O_CLOEXEC`。

## 推迟项

- 读出来的路径仍是执行时的，程序文件改了名不跟着变（与 fd 一样，[第 93 轮](93-fd-paths-resolve-symlinks.md)的推迟项）。
- 经由 fd 执行已经删除的文件和 memfd（glibc 的 fexecve、`execveat` 的 `AT_EMPTY_PATH`、`execve("/proc/self/fd/<n>")`）：原生上四种都能执行；tuxport 里 memfd 的 fexecve 是 ENOENT（object 在创建时就删了，execve 按 inode 重新打开 object），经由 `/proc/self/fd/<n>` 是 EACCES（execve 不接受 `OpenFd`），已经删除的普通文件的 fexecve 只在删除还没提交时碰巧成功。做法可以与这一轮的 `OpenExe` 一样，按打开文件记着的宿主文件执行；另外 Linux 上 memfd_create 得到的 fd 不算写者（`alloc_file_pseudo` 不增加 `i_writecount`），执行它不是 ETXTBSY，tuxport 的检查会把它当作以写方式打开着。用到它的多是 runc、systemd 这类 D 档的程序。→ 已补上（M9 之前：经由 fd 执行 memfd 和已经删除的文件）：runc、buildah 的 chroot 隔离都要用
- core dump 的 `%E` 不带 ` (deleted)`（Linux 的 `cn_print_exe_file` 用 `d_path`，带）。
- `PR_SET_MM_EXE_FILE`（换掉 exe，CRIU 用）没有实现。
