# M9 之前：线程退出时在 BKL 外丢掉弱引用（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 2：线程频繁开始、退出时宿主的堆被破坏，整个 tuxport 崩溃。Node、Go、ripgrep 的测试和 Ruby 的连续运行都被它挡住了。

## 原因

就是普查那一篇读代码找到的那一处。线程局部变量 `CURRENT` 记着宿主线程正在执行的 guest 线程和它的线程组（`Weak<Task>`、`Weak<RefCell<ThreadGroup>>`），`task::thread_main` 在 `run` 返回、BKL 已经放开之后才清掉它。Rc 的强弱计数不是原子的。这时同一个进程里的另一个线程可能正持有 BKL，开始运行时 `Rc::downgrade(&group)` 给弱引用计数加一，或者丢掉别的引用。两边的读改写交错就丢掉一次修改，计数偏小，`ThreadGroup` 的分配在还有人指着它时就被释放，之后的读写都落在释放掉的内存上：glibc 报 `corrupted double-linked list`，有时先在定时器线程、wait4 里出现 "RefCell already borrowed" 的 panic。`CURRENT` 是 [signalfd 看调用的线程](52-signalfd-caller.md)那一轮（444e2fd）加的，从那时起就有这个竞争。[终端的属主和权限](64-pty-owner.md)那一轮 exit_group01 以 SIGABRT 失败的那一次多半就是它。

## 做法

`exit` 的最后一步在持有 BKL 时清掉 `CURRENT`，`thread_main` 不再碰它；`CURRENT` 只在持有 BKL 时设置和清掉。内核层别的线程局部变量（`SIGNALS`、`PENDING`、`EVENT`、`UNMAPPED`）都不持有 Rc。

## 测试

- `tests/guest/churn.c`：几个进程里的几个线程不停地创建、join 短命的线程，改动之前每次都崩溃，之后跑 60 秒都正常。cargo test 新增 `thread_churn_keeps_the_host_heap_intact`，在改动之前的代码上失败。
- Ruby 的测试 `-j4` 跑完了，用时 1781 秒（原生 2 分钟），只在 tuxport 上失败的与上一轮逐个文件跑时相同。

问题 7 另有原因：Ruby 的测试跑完之后 tuxport-run 一直不退出，后面的 ripgrep、Node、Go 都没有开始。剩下 3 个 guest 线程，从测试开始后第 3 分钟起各占满一个 CPU，到发现时一个半小时，用 gdb 看，都停在分发器把停放的块放回翻译表时找空位的循环里：表里 131072 项全部占满，十二万项是作废的块留下的墓碑，而计数里只有六万八千个。上一轮 `-j4` 时四个工作进程不推进也是这样。这是 Tier 1 翻译表的 bug，与这一轮的竞争无关，[下一轮](72-table-tombstones.md)修；Node、Go、ripgrep 的测试修完它再重跑。

## 推迟项

- Node.js、Go 标准库、ripgrep 的测试还没重跑，被翻译表的 bug 挡住了。→ 已重跑（M9 之前：翻译表里漏掉的墓碑）：ripgrep、Go 与原生相同，Node 多出的失败是 OOM 和两个新问题
- 只查了线程局部变量。在 BKL 外丢掉 Rc 的别的路径（例如值活过了放开 BKL 的那一刻）没有逐个排查；churn 和 Ruby 的测试都没有再出现堆的破坏。→ 已排查（M9 之前：排查在 BKL 外改 Rc 的计数）：正常执行时没有；放开 BKL 期间 panic 时展开经过的外层各帧在锁外丢 Rc，已改为先重新拿到 BKL
