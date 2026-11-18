# M9 之前：cgroup 的 pids.max 和 memory.max 起作用（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 9 的第二步的前一半：[上一轮](163-cgroup-tree.md)的限制只按格式记下，这一轮让 `pids.max`、`memory.max` 起作用（`docker run --pids-limit`、`--memory`，podman 的同名选项，systemd 的 `TasksMax=`、`MemoryMax=` 写的就是它们）。`cgroup.freeze` 要做透明的冻结和系统调用重启，下一轮单独做。照 Linux 6.1 的 `kernel/cgroup/pids.c`、`mm/memcontrol.c`、`mm/oom_kill.c`。

## 做法

- **pids.max**（`cgroup::pids_charge`，Linux 的 `pids_can_fork`）：fork、clone、clone3（含建线程、`CLONE_INTO_CGROUP` 放进别处的）之前，新任务要去的 cgroup 和它的每个开了 pids 的祖先，任务数加 1 都不能超过 `pids.max`，超过是 EAGAIN，放在 RLIMIT_NPROC、threads-max 的检查之后。
  - 任务数（`pids.current`）与 Linux 一样到回收为止都算：进程的活着的线程，主线程已经退出的（僵尸进程、主线程先退出的进程）再加 1。根的原来是线程数，现在也这样算。
  - 失败记进离新任务的 cgroup 最近的开了 pids 的 cgroup 的 `pids.events`（6.1 的语义：不往上加，新内核改成往上加、另有 `.local`），第一次时内核日志记 `cgroup: fork rejected by pids controller in <路径>`。
  - 迁移进来不看上限（`pids_can_attach` 只是记账），`pids.current` 可以超过 `pids.max`。
  - `pids.max` 的范围改成 0 到 2^22（Linux 的 `PIDS_MAX` 是 `PID_MAX_LIMIT + 1`，上一轮少了一个）。
- **memory.max**（`oom.rs`）：OOM killer 原来只量整个 App，现在也量设了 `memory.max` 的 cgroup（子树里的进程的驻留内存之和，与上一轮的 `memory.current` 相同）。超出时与 Linux 的 memcg OOM 一样：
  - 那个 cgroup 记一次 `max`，从它下面按 oom_badness 挑一个进程（`oom_score_adj` 按 cgroup 的上限折算，-1000 的不挑）结束，记 `oom`，被结束的进程所在的 cgroup 记 `oom_kill`。上次挑中的进程还没退完时等它，不再记 `max`，也不另挑（它的内存马上就会还回来；它停着不分配，Linux 也不记）。
  - 事件按 `memcg_memory_event` 记：自己的 `memory.events.local`，自己和祖先的 `memory.events`。App 的上限的事件记在根上，原来 `Oom::events` 的那一份去掉了。内核日志的 `oom_memcg=`、`task_memcg=` 写真实的路径（原来总是 `/`）。
  - `memory.oom.group`：被结束的进程所在的 cgroup 到超出的 cgroup 之间、设了它的最上面那个，里面的进程一起结束（-1000 的除外），记 `oom_group_kill`；与 Linux 的 `mem_cgroup_scan_tasks` 一样，挑中的那个在 `oom_kill` 里又算一次。
  - 调低 `memory.max` 时马上量，已经超出就结束一个（Linux 的 `memory_max_write` 回收不了时 OOM，这时不记 `max`）；之后的照常定时检查。写了 `memory.max` 之后叫醒定时器线程马上检查一次。
  - 子 cgroup 的 `memory.peak` 是量到过的最大用量（定时检查和读 `memory.current`、`memory.peak` 时取样），原来就是当时的用量。
  - 量 cgroup 要对每个进程的已写过的映射做 mincore，在定时器线程里拿着 BKL：实测几十 MB 的进程一两百微秒，node 占 185 MB 时 0.8–1.6 毫秒，1000 个 1 MB 映射的进程 0.3–2 毫秒。所以只量设了上限的 cgroup，检查的间隔至少是这次耗时的 20 倍，占 BKL 的时间不超过 5%；代价是大的 cgroup 超出之后发现得晚一些。
- **关掉控制器**：父 cgroup 的 `subtree_control` 关掉一个控制器时，子 cgroup 里它的接口文件的值、`pids.events`、内存的事件和峰值都丢掉（Linux 销毁那个 css），再开时是默认值。

## 测试

- 新的差分测试组 `cgroup-limits`（原生一侧是特权容器）：pids 的计数、上限、`pids.events` 和迁移，memory 上限里分配、调低上限、`memory.oom.group` 时进程被结束和各级的事件，两个执行层下都与原生逐字相同。容器的根有进程时开不了控制器，用例先像 docker-in-docker 那样把进程挪进 `init`；`max` 和调低上限那次的 `oom` 只比是否大于 0（Linux 每次碰到上限都记）。
- 写用例时发现两处与 Linux 不同，都改了：调低 `memory.max` 时 Linux 在写的时候就结束进程；Tier 1 下同步结束进程之后，定时检查在它退完之前又记了一次 `max`。仓库里的 `/tmp` 是持久的，上次留下的 fifo 要先删。
- 等着被结束的进程最多等 10 秒，内核不对时用例失败而不是卡住（第一次做变异检查时卡住了）。
- 在 tuxport 里实际跑：podman 带 `--pids-limit`、`--memory` 时建好了容器的 cgroup，停在 crun 挂设备过滤的 eBPF 程序（`bpf create` 是 ENOSYS），与 docker 一样要等下一步。

## 推迟项

- `memory.max` 是定时检查的：不停分配的进程在被发现之前会超出几十 MB（大的 cgroup 检查得更稀，超得更多），`memory.peak` 也就超过上限；Linux 在分配时就挡住，峰值正好是上限。调低 `memory.max` 时 Linux 等到用量降下来才返回，这里结束进程之后马上返回。
- `memory.current` 是各个进程的驻留内存之和：fork 之后共用的页、共享的映射在每个进程里各算一次，没有页缓存、内核内存；Linux 每页只记在第一次用它的 cgroup 上。
- `memory.high`（限速回收）、`memory.low`、`memory.min`（保护）、`memory.swap.max`、`memory.swap.high` 只记下，不起作用，`memory.events` 的 low、high 总是 0；没有 `memory.pressure`。
- `cpu.max`、`cpu.weight`、`cpuset.cpus` 不起作用：宿主线程的调度不归 tuxport 管（要做的话可以按周期停下超额的线程，与冻结用同一套办法）。
- `cgroup.freeze`：下一轮。→ 已做（M9 之前：cgroup.freeze）
