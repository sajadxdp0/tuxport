# M9 之前：被叫醒的进程在 /proc 里是 R（2026-10-28）

LTP 的 shmctl01 记在[已知的偶发失败](../milestones.md#known-flaky-tests)里，说明是"全量并发时子进程还没 `shmdt` 完就超时"。按时间排列保存下来的全量结果，它早期时好时坏，最近 11 次每次都失败，日志里多数不是超时：子进程 shmdt、父进程检查时 `shm_nattch` 还是 13（应该是 0），继承的那段也一样。

## 原因

shmctl01 的 20 个子进程停在 `pause()` 里。父进程给它们各发一个 SIGUSR1，再用 `TST_PROCESS_STATE_WAIT` 轮询 `/proc/<pid>/stat`，等状态回到 S，就当子进程已经做完了下一步（shmat 或 shmdt），接着检查 `shm_nattch`。Linux 上 kill 返回之前，`signal_wake_up` 经由 `try_to_wake_up` 已经把被叫醒的进程改成可运行，状态是 R，直到它做完这一步、再进 pause 才回到 S。tuxport 的状态看的是线程的 `sleeping` 标记，它在线程等待时置上，被叫醒的线程重新拿到 BKL 之后才清掉。这中间状态还是 S，父进程马上就往下走，有的子进程还没跑起来。并发的负载越重，子进程起来得越慢，所以最近几乎每次都失败。

## 做法

所有叫醒最后都落到等待线程自己的事件（eventfd）上，放开 BKL 时触发。`Task::asleep` 是 `sleeping` 置着、并且事件还没触发；事件已经触发、线程还没清掉它时就是"被叫醒了还没跑起来"，`/proc/<pid>/stat`、`status` 等报告 R。hal 的 `Event` 加了不清除状态的 `is_signaled`（零超时的 poll）。平均负载、OOM 检查、挑接收信号的线程仍然只看 `sleeping`。

## 测试

- 差分测试 compat 组加了 `woken.c`：子进程停在 `pause()` 里，父进程照 LTP 的做法发 SIGUSR1、等它们的状态回到 S，检查它们都已经做完这一轮，两层都与原生相同。
- LTP 的 shmctl01 和 shm、msg、sem 的测试一起并发跑，改动之前每次都失败，之后都通过；全量里新通过 shmctl01。
- hal 的单元测试新增 `checking_an_event_does_not_clear_it`。

## 推迟项

- 被叫醒的线程在平均负载里仍算作睡着，直到它拿到 BKL。
