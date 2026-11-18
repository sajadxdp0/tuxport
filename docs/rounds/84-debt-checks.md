# M9 之前：补测待查的几项（2026-10-28）

把各篇推迟项里"没有查""没有对照""没跑"的几项补测完。只改了一个普查脚本，内核层不用改。补测时发现的两个问题已经在前几轮修好：shmctl01 的状态同步（[第 81 轮](81-woken-state.md)），Debian、Ubuntu 上 PAM 登录不了（[第 83 轮](83-netlink-protocols.md)）。

## 结果

- **CPython 的 test_os 第一遍失败一个**（[第二轮普查](70-survey-suites.md)）：失败的是 `test_timerfd_ns_TFD_TIMER_ABSTIME`，它要求读 `CLOCK_REALTIME`、以绝对时间设 timerfd、再读剩余时间这三步之间不超过 1 毫秒，那次多了 2.77 毫秒（`-j4` 一起跑时）。照这三步写了个脚本（`target/debt/timerfd_gap.py`，每次 5000 遍）：空闲时 tuxport 的中位数 1.3 微秒（原生 0.7），没有系统性的偏差；4 个核被 6 个空转循环占满时，tuxport 3 次里超过 1 毫秒的有 3、3、2 遍，原生有 1、1、0 遍，最长的都是 3 毫秒左右（被调度出去一个时间片）。是这个测试对负载敏感，不是 tuxport 的问题。
  - 量的时候第一遍忘了传 `--tier 1`：`tuxport-run` 默认用解释器，这三步要 35 微秒。
- **Ruby 的 `*_memory_leak` 超时**（第二轮普查）：单独跑 4 个文件里的 16 个 memory_leak 测试，默认时限（10 秒）下 3 个超时；`--timeout-scale 5` 时只剩 `TestModule#test_complemented_method_entry_memory_leak`，放宽到 15 倍时 139 秒跑完，5 个断言（包括内存不增长）都通过。这个测试把"建类、建 refine 模块、取方法"重复 30 万次，单独量 3 万次：tuxport 13.3 秒，原生 4.65 秒（约 2.9 倍，与别的负载相当），所以它在这台虚拟机上原生也要 46 秒左右，第二轮普查原生一侧唯一的超时就是它。只是慢，没有泄漏。`tests/survey/suites/ruby-tests.sh` 改成带 `--timeout-scale 5` 运行（原生一侧也一样）。
- **VS Code 的第一个终端丢字**（[glibc 发行版的探测](69-glibc-probe.md)）：tuxport 和原生的 Debian 13 各起一个 code-server，在 Chrome 里打开一个新的工作区，按 Ctrl+\` 打开第一个终端，紧接着输入 `ZYXWVUTSRQPONMLKJIHGFEDCBA; history 1 > /tmp/typed-N` 和回车（前面 26 个字母是缓冲，丢了几个字符后半句照样执行，bash 的 `history` 把实际收到的那一行写进文件）。两边各做 2 次：原生 2 次都只收到 `history 1 > /tmp/typed-N`，前 28 个字符都丢了；tuxport 2 次都完整收到。第三次两边都没打开终端（原生弹出了"是否信任这个文件夹"，tuxport 的页面 10 秒内没加载完），不算。终端起来之前的输入 VS Code 不一定收下，原生上一样，不是 tuxport 的问题。
- **conda**（glibc 发行版的探测）：新增 `tests/survey/glibc/debian-conda.sh`，在 Debian 的仓库里装 Miniforge（conda 26.9），建一个 Python 3.12 加 numpy 2.5 的环境，用它算求和、行列式，再 `conda run`。tuxport 上都正常，结果与原生的 `debian:trixie` 相同；建环境 26 秒，原生 15 秒。没实现的 syscall 只有 235（mbind）和 293（rseq），与 Linux 没配对应功能时一样返回 ENOSYS。
- **Ubuntu 上更多的负载**（glibc 发行版的探测）：Ubuntu 24.04 的仓库里跑了 su 和 `debian-sshd.sh`。su、sshd 的登录在第 83 轮之前都不能用，现在正常；会话里 `who -u` 正常，`w -h` 段错误，原生的 `ubuntu:24.04` 上同样段错误，是 Ubuntu 这一版 procps 的问题。
- **Tier 0 下的普查**（glibc 发行版的探测）：Alpine 的普查里挑了 12 个负载（去掉 rust、dotnet 这两个最重的），先用 Tier 1、再用 Tier 0 各跑一遍，都正常退出。输出比对下来只差用时、端口号、redis 和 postgres 的吞吐，以及 rsync 的目标目录在前一遍已经同步过（`sshtools.sh` 没有删掉 `/tmp/rs-etc`），没有执行层造成的差异。Tier 0 下 tools（里面编译一个 C 程序）295 秒、postgres 106 秒，Tier 1 下 20 秒、19 秒。
- **普查不和原生自动比对，也不进 CI**（[负载普查](62-workload-survey.md)）：决定不做。普查的输出里有时间、pid、端口、吞吐和各种日志，要逐个负载规整才能比对；进 CI 要联网装约 1 GB 的软件包、跑半个小时以上。一直以来的做法是普查找到的每个问题修好时都在差分测试或 cargo test 里加一条回归检查（这一轮的 shmctl01、netlink 也是），自动化的那一层由它们承担；普查留作人看的工具，第二轮起有 `-n` 跑原生对照。
- 都只在 colima 的虚拟机里跑，仍然没上手机。

## 推迟项

无。
