# M9 之前：fork 之后还回的页按各自的权限恢复（2026-10-28）

[负载普查](62-workload-survey.md)的问题 1：rustc 生成代码的编译每次都在退出时 SIGSEGV。这是[fork 写时复制](26-fork-cow.md)那一轮就有的 bug，任何 fork 过的进程都可能撞上。

## 原因

fork 时写过的私有匿名页借给子进程，父进程的页表项去掉写权限；子进程 exec 或退出时还回来，最后一个借用方还回之后，父进程的 `Mm::repossess` 恢复写权限。它先用 `Runs` 把客户机地址和宿主地址都连续的页接成一段，再整段改成第一页所在 VMA 的权限。可宿主内存连着的页不一定在同一个 VMA 里：mprotect 切开的几个 VMA 共用一段宿主内存，宿主的 mmap 自顶向下分配，相邻的两次 guest mmap 宿主地址往往也相邻。

- 一段从保护页（PROT_NONE）开始时，后面的页都变得不能访问。rustc 的主线程有一个开头是保护页的信号栈，紧挨着上面是 malloc 早先 mmap 的两页（atexit 链表的节点在里面）。rustc fork 出的链接器 exec 之后，这十几页都成了 PROT_NONE，退出时 `__funcs_on_exit` 一读就崩。Tier 0 也一样，与翻译无关。
- 一段从可写的页开始时，后面只读的页变得可写：程序写只读内存不再收到 SIGSEGV。
- 会不会撞上看映射恰好怎么排。普查第一次时 rustc 没有撞上，之后每次都撞上。

## 怎么找到的

core 文件显示崩在 musl 的 `__funcs_on_exit` 里，读的那一页是 malloc 早先 mmap 的。`TUXPORT_STRACE` 里 rustc 自己没有再碰过那一页。于是给 engine 的 `Mmu` 的 map、unmap、protect 临时加上监视：碰到指定的页时打印调用栈。打出来的是子进程 execve 销毁地址空间时，经由 `Mm::drop` 进入父进程的 `repossess`，执行了 `protect 0x7fffbf7e2000+0xf000 prot=0`。监视的代码查完就撤掉了。

## 做法

`Runs` 记下每段所在的 VMA，只在同一个 VMA 里接段；`repossess` 按那个 VMA 的权限恢复。fork 时借出（`Mm::lend`）本来就逐个 VMA 处理，只是跟着改了接口。

## 结果

只是接段时多比较一次 VMA 的起点，段只在 VMA 的边界多断开，fork 和还页的开销不变，没有另外量。

## 测试

- 差分测试 `fork` 新增一节：几次 mmap 用 mprotect 切成权限不同的几段（包括像信号栈那样开头是保护页、接在上一段下面的），写过之后 fork，子进程退出，父进程逐页试读写，结果与原生相同。
- 单元测试 `returned_loans_restore_each_vma_its_own_protection`：几个权限不同的 VMA 在子进程还页之后各是各的权限。
- rustc 的 release 编译连续 10 次都成功（之前每次都崩），普查的 `rust` 负载通过。

## 推迟项

无。
