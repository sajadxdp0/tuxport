# M9 之前：Ubuntu 26.04 的普查（2026-10-31）

[termios2](145-termios2.md)和[openat2](144-openat2.md)那两轮的问题都是新版的用户态才会碰到的：App 里现在的三个发行版（Alpine、Debian 13、Ubuntu 24.04）测不出来。这一轮拿最新的 LTS（Ubuntu 26.04.1：glibc 2.43，coreutils 换成了 Rust 写的 uutils 0.10）当仓库跑一遍常用的命令，与同一个仓库导出的原生容器比。

## 做法

- 仓库：docker 的 `ubuntu:26.04` 导出成 `target/glibc/ubuntu-26-04.tar.gz`，导入 `target/glibc/ubuntu26-store`（命令写在负载的开头）。
- 负载 `tests/survey/glibc/ubuntu26-install`：apt 装 python3、gcc、git、sudo、openssh、tmux、screen、strace、gdb、util-linux 这些（238 个包），打印几个主要程序的版本。
- 负载 `tests/survey/glibc/ubuntu26-tools`：uutils 的各个命令（ls、stat、cp -a、mv、readlink、touch -d、chmod/chown -R、du、df、sort、seq、cut、numfmt、sha256sum、dd、split、mktemp、install、timeout、tee、mkfifo、date、nice、stty、tty……），find/xargs、grep、sed、awk，tar 配 gzip、xz、zstd，ps、free、uptime、pgrep/pkill、lscpu、findmnt、script、getent，ip、ping，sudo、su，gcc 编译多线程程序（顺带 isatty、tcgetattr），python 的线程池、ssl、sqlite3、multiprocessing、venv，git，ssh-keygen，tmux，screen，PTY 上的 stty、tput、带 readline 的 bash。输出不带时刻、pid，用 `scripts/survey -n` 和从仓库导出的原生镜像比。
- 都在开发用的虚拟机里跑，没上手机。

## 结果

- **tar 已经离不开 openat2**：Ubuntu 26.04 的 tar（1.35+dfsg-4ubuntu0.4）解包带子目录的归档时逐级 `openat2(..., RESOLVE_BENEATH)`（strace 看到的调用与原生逐字相同），就是[openat2](144-openat2.md)那一轮读源码时说的新版本；那一轮之前 openat2 是 ENOSYS，这种解包会失败。
- 安装和绝大多数命令与原生相同。PTY 上 uutils 的 stty（经由 glibc 2.43 的 tcgetattr、tcsetattr，即 `TCGETS2`、`TCSETS2`）、tput、带 readline 的 bash 都正常，[termios2](145-termios2.md)之前都不行。未实现的系统调用只有以前记过的 293（rseq）、444（landlock，xz 的沙箱）、452（fchmodat2，退回 `O_PATH`）。
- 与原生不同、属于环境的：`df`（tuxport 报告仓库所在的宿主文件系统）、`/etc/hosts`（docker 运行时挂上自己的）、ping 的时间。
- 找到的问题：
  1. **GNU screen 时好时坏**：普查里 screen 的窗口没有运行，单独跑时多半正常。原因是主设备的读在从设备从没打开过时就返回了 EIO（也报 POLLHUP）：screen 的子进程还没打开从设备时，主进程读主设备得到 EIO，就当窗口已经结束了。Linux 只在从设备打开过、又全部关掉之后（`TTY_OTHER_CLOSED`）才这样，之前照常阻塞（原生上确认过：已解锁、没打开过时读是 EAGAIN、没有 POLLHUP）。tuxport 的文档注释写的正是"打开过又全部关闭之后"，实现漏了"打开过"。→ 已改（M9 之前：新分配的 PTY）
  2. **符号链接报了数据块**：`ls -l` 的 total 比原生多 4。tuxport 对所有文件按大小向上取整到 4 KB 报 `st_blocks`，ext4 上目标短于 60 字节的符号链接存在 inode 里，块数是 0（tmpfs 是 128 字节以内）。→ 已改（M9 之前：短的符号链接不占块）
  3. **新分配的 PTY 的窗口大小**：`script` 在 stdin 不是终端时不设窗口大小，里面的 `stty -a` 在原生上是 `rows 0; columns 0`，tuxport 是 24、80：tuxport 新建终端时的窗口大小默认 24×80（给控制台用的），Linux 新分配的终端是全 0。→ 已改（M9 之前：新分配的 PTY）
- 顺带看到的，与原生相同：Ubuntu 26.04 的 `tty`（uutils）输出不带结尾的换行；apt 装 `sudo` 装上的是 C 写的 sudo 1.9.17（`/usr/bin/sudo.ws`），sudo-rs 没有测到；tar 是 GNU tar 1.35。
