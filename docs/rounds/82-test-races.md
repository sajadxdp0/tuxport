# M9 之前：测试自己的竞争和丢掉的现场（2026-10-28）

几处"原因没查明"的记录，一处是测试自己的竞争，两处是失败的现场被重跑覆盖了、事后查不了。这一轮只改测试和脚本，不改内核层。

## 做法

- **作业控制的用例卡住**（[起进程的宿主开销](28-spawn-host-cost.md)那一轮记下的）：`ctrl_c_and_ctrl_z_control_the_foreground_job` 卡住过一次，作业停着、shell 在等输入，测试在等它退出。
  - 原因在测试：busybox ash 的 `fg` 先打印作业的命令并 flush（`showpipe`），之后才在 `restartjob` 里关中断（`INT_OFF`）、`tcsetpgrp` 把终端交给作业、发 SIGCONT。测试一看到打印出来的命令就按 ^C，这时前台进程组如果还是 shell 自己，SIGINT 就落到 shell 上：ash 的 `raise_interrupt` 直接跳回提示符，`$?` 设成 130，作业一直没被继续。测试接着看到的提示符、`status 130` 都对，最后 `exit` 时 ash 因为还有停住的作业不肯退出，而测试等 shell 退出时没有超时，就一直等下去。测试里原来的注释"fg 打印作业的命令之前已经把终端交给了它"是错的。原生 Linux 上按得足够快也一样，不是内核层的问题。shell 打印完到 `INT_OFF` 只有几条指令，要宿主恰好在这几条指令之间把它换下去，而且换下去的时间够测试看到输出、按下 ^C（测试每 10 毫秒看一次输出），所以那一轮跑的 16 次全量 cargo test 里只碰到一次。
  - 只靠负载很难撞上：`target/debt/jobctl.py` 照着用例的步骤驱动交互 shell，4 个核都被空转循环占满时原来的写法 120 次、把 tuxport 和 6 个空转循环绑在同一个核上时 30 次，都没有卡住。改用 strace 的 `inject=write,writev:delay_exit=300000` 把 shell 每次输出的返回推迟 300 毫秒（`target/debt/jobctl-native.py`），数据已经写进终端、shell 还停在 writev 的出口，这时按 ^C 就一定落在这个窗口里：原生的 Alpine 上和 tuxport 里结果相同，提示符和 `status 130` 都对，`jobs` 里作业还停着，`exit` 打印 "You have stopped jobs." 不退出，正是那次看到的现场。改过的写法在同样的推迟下两边都正常退出。
  - 改动：作业改成从终端读行的循环（`while read x; do echo got-$x; done`）。`fg` 之后先输入一行，看到作业回显 `got-ping`，说明它已经在前台运行了，再按 ^C。等 fg 打印的命令时只认 `; done`：ash 打印时把单引号换成了双引号，`$` 前面加了反斜杠（第一版等的是 `do echo got-$x`，对不上）。顺带让停在读终端上的作业被 ^Z 停住、继续之后接着读也有了覆盖。`Interactive::exit` 等 shell 退出最多 30 秒，超时就带着输出失败，不再一直等。
- **差分测试的输出被覆盖**（[FIFO 在写端来之前不报 POLLHUP](76-fifo-pollhup.md)那一轮的 Tier 0 失败）：`scripts/difftest` 把两边的输出写到 `target/difftest/<名字>.native`、`.tuxport`，下次运行时覆盖。现在不一致时另存一份到 `target/difftest/failed/<时刻>-t<执行层>-<名字>.*`。那一次的失败没有留下输出，查不了，以后再出现时有现场。
- **LTP 的日志被覆盖**（[终端的属主和权限](64-pty-owner.md)那一轮 exit_group01 的 SIGABRT）：`scripts/ltp` 每个测试的输出写到 `logs/<名字>.log`，失败之后单独重跑就把它覆盖了。现在写之前把上一次的改名成 `<名字>.prev.log`。

## 测试

cargo test（207 个）、clippy 通过；改过的用例（两层）单独连跑 10 次都通过，上面的驱动脚本照它的步骤在满载下又跑了 150 次。全量 LTP 用改过的脚本跑了一遍（[上一轮](81-woken-state.md)的那次），每个测试都留下了 `.prev.log`。

## 推迟项

无。
