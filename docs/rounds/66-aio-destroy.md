# M9 之前：io_destroy 叫醒等事件的线程（2026-10-28）

[负载普查](62-workload-survey.md)的问题 4：`mariadb-install-db` 永远不结束，之后启动的 mariadbd 也因为拿不到数据文件的锁（还被卡住的那个占着）起不来。

## 原因

InnoDB 用 Linux 原生 AIO，一个线程专门在 `io_getevents` 里等完成事件。关闭时主线程 `io_destroy`，再 join 这个线程：Linux 的 `kill_ioctx` 把上下文标成已销毁并叫醒等它的线程，`aio_read_events` 看到 `ctx->dead`，这次一个事件也没取到就返回 EINVAL，线程据此退出。tuxport 的 `io_getevents` 一开始取了上下文的引用就一直拿着循环等；`io_destroy` 只把上下文从进程的表里拿掉、叫醒所有等待者，等待者醒来看到的还是那个上下文，接着等，主线程的 join 就永远等下去。这一点从 [M9 之前：推迟项](19-deferred.md)那一轮实现 AIO 起就是这样。

## 怎么找到的

`TUXPORT_STRACE` 里 mariadbd 的主线程最后一个返回的系统调用是 `io_destroy`，等事件的线程停在 `io_getevents` 里；对照 Linux 6.1 的 `fs/aio.c` 就清楚了。

## 做法

上下文记一个 `dead`，`io_destroy` 设置它，并取消挂起的 `IOCB_CMD_POLL`（不产生完成事件，与 Linux 相同）。`io_getevents`、`io_pgetevents` 在等待中发现上下文已经销毁时，已经完成的事件照样取走，一个也没有就返回 EINVAL。

## 测试

- 差分测试 `compat` 的 AIO 一节新增：别的线程在 `io_getevents` 里等着时销毁上下文，没有事件和已经完成一个的两种情况。等待者最多等 5 秒（`pthread_timedjoin_np`），免得测试本身卡住。
- 普查的 `mariadb`：install-db 能结束，InnoDB 用原生 AIO 插入、回滚、关闭都正常。

## 推迟项

无。
