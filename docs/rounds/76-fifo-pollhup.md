# M9 之前：FIFO 在写端来之前不报 POLLHUP（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 3：FIFO 的读端在还没有写端连上过时，poll 报 POLLHUP。Ruby 的 `load` 一个 FIFO 时以 `O_NONBLOCK` 打开再 poll，在 tuxport 上立刻读到 EOF，当作空文件 load 成功，test_require 的三个 FIFO 测试因此失败。

## 原因

Linux 6.1 的 `pipe_poll` 只在"现在没有写端，而且这个读端的 `f_version` 和管道的 `w_counter` 不同"时才报 EPOLLHUP。`fifo_open` 以 `O_NONBLOCK` 只读打开、当时又没有写端时，把读端的 `f_version` 记成当时的 `w_counter`（注释是"在见到写端之前不报 EPOLLHUP"），写端每打开一次 `w_counter` 加一。其余情况 `f_version` 是 0，没有写端就报。tuxport 只看现在有没有写端。

## 做法

以 `O_NONBLOCK` 只读打开 FIFO、当时没有写端时，读端记下写端被打开过的次数（管道原来就有这个计数，阻塞的 open 用它等对端）；poll 在没有写端、并且这个次数没变时不报 POLLHUP。read 不受影响，没有写端时照样立刻返回 0，与 Linux 相同。

## 测试

- 差分测试 compat 组加了 FIFO 在写端来之前、期间、离开之后的 poll 结果，两层都与原生相同。
- Ruby 的 test_require 全部通过。
- 第一次跑整套差分测试时 Tier 0 有一组失败，之后重跑都通过，失败的是哪一组已经被重跑的输出覆盖了，没有查到原因。

## 推迟项

- Tier 0 差分测试那一次失败没有查明（见上）。→ 输出已经没有了，查不了；差分测试从此在失败时另存两边的输出，再出现时有现场（M9 之前：测试自己的竞争和丢掉的现场）
