# M9 之前：`/proc/<pid>/fd` 和 maps 里的路径解析符号链接（2026-10-29）

[上一轮](92-debuggers-valgrind-chromium.md)在 Debian 上跑 valgrind，memcheck 报了几千个"未初始化"的错误（`ERROR SUMMARY: 3421 errors from 313 contexts`，应该是 3 个），开头有两行 `When reading debug info from /lib/ld-linux-aarch64.so.1: can't open file to inspect ELF header`。跟进子进程的 sh、ls 和 python 也都报了一大堆。

## 原因

valgrind 自己加载客户程序和解释器，记下映射的文件名时读 `/proc/self/fd/<n>`，之后读调试信息时按这个名字 stat，再带着 `O_NOFOLLOW` 打开。

- Linux 的 `d_path` 给出解析完符号链接的路径 `/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1`。tuxport 记的是打开时传进来的路径，只做词法规范化，得到的是 `/lib/ld-linux-aarch64.so.1`：Debian 的 `/lib` 是指向 `usr/lib` 的链接，最后一级也是链接。`O_NOFOLLOW` 停在链接上，结果是 ELOOP。
- 读不到 ld.so 的符号，valgrind 就替换不了 ld.so 里按字读的 `strlen` 这些函数。它们会读到字符串末尾之后的字节，每次都报未初始化。
- `/proc/<pid>/exe` 在 [M8 补测 JVM](11-m8-jvm.md) 时已经改成解析后的路径（`Vfs::real_path`），打开的文件和解释器的映射一直没改：[M3b](05-m3b.md) 记的推迟项当时只补了 exe。

## 做法

- 打开文件时记下的路径（`/proc/<pid>/fd/<n>` 显示它，mmap 之后 maps 里的名字也取它）改成按查找路径时最后找到文件的目录和名字算，与 `real_path` 用同一个函数（`Vfs::found_path`）。具体有这几种情况：
  - 符号链接都解析；
  - `..` 按实际的目录回退（原来按词法，链接后面的 `..` 回到链接所在的目录）；
  - `O_PATH | O_NOFOLLOW` 停在链接本身时，是链接的路径；
  - 沿着悬空的链接新建的文件，是链接指向的路径；
  - 经由 `/proc/<pid>/fd/<n>` 这种链接到达、不知道名字的文件，仍按词法规范化的路径，与原来相同。
- execve 时解释器在 maps 里的名字原来是 PT_INTERP 原样的字符串，现在也解析符号链接。起第一个程序（`Machine`）和 execve 共用 `interp_label`，名字和 inode 只查找一次（原来为了 inode 另外查找一次）。

## 结果

只在两处。打开文件时，每次多算一遍找到文件的那个目录的路径，查的是元数据缓存；execve 时，解释器少查找一次。用 open 的微基准（`target/mem/openbench.c`，同一个文件开关 20 万次）与上一轮的二进制交替跑：

- 绝对路径 0.374–0.390 → 0.392–0.394 秒，每次多约 0.05–0.1 µs；
- 相对当前目录的路径 0.379–0.391 → 0.359–0.361 秒，原来要先算当前目录的路径、再做词法规范化；
- 经由链接的路径 0.352–0.360 → 0.352–0.361 秒；
- python、gcc、shell 起进程、`git status`、`go build`、Maven 都在噪声范围内。

## 测试

- cargo test 新增 `procfs_fd_paths_resolve_symlinks`，两层都跑：最后一级是链接、链接后面的 `..`、沿着悬空链接新建的文件、经由链接进入的当前目录下的相对路径。
- 差分测试 compat 组新增一个 C 程序，另外检查 `O_PATH | O_NOFOLLOW`、经由链接打开的目录和 mmap 的文件、PT_INTERP 指向链接的程序里解释器的映射，与原生逐行相同。
- 上一轮的 valgrind 负载：memcheck 只报出程序里故意留的 3 个错误，跟进的子进程和 python 都没有。

## 推迟项

下面三处原来就和 Linux 不同，都是因为 tuxport 在打开时就算好路径，Linux 是读的时候按 dentry 算：

- 打开之后删除的文件，Linux 的路径后面有 ` (deleted)`，这里没有（memfd 有）。→ fd 和 maps 已补上（M9 之前：已经删除的文件带 ` (deleted)`），exe 后来也补上了（M9 之前：/proc/<pid>/exe 指向程序文件本身）
- 经由 `/proc/<pid>/fd/<n>` 重新打开的文件，Linux 显示原来那个文件的路径，这里显示 `/proc/<pid>/fd/<n>`。
- 打开之后文件或上级目录改了名，Linux 显示新的路径，这里仍是打开时的。
