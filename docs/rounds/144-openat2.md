# M9 之前：openat2（2026-10-31）

[未实现的系统调用](139-unimplemented-syscalls.md)那一轮记下 openat2 返回 ENOSYS。第 143 轮之后读源码查了谁在用（结论记在那一条推迟项下面）：多数调用者在 ENOSYS 时退回逐级打开，硬失败的是 GNU tar 的新版本（上游 git 从 2025-11 起、Fedora 44 已带）配 glibc ≥ 2.43，解包带子目录的归档全部失败；App 里现在的三个发行版不受影响，换到 glibc 2.43 和新 tar 之后会。差分测试的仓库里的 bubblewrap 0.12 也用它（`RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`），原来走退路。Android 16 及以前的应用 seccomp 不放行 openat2，不能交给宿主内核，在 tuxport 自己的路径解析里做。

## 做法

- **系统调用**（`syscall/fs.rs` 的 `openat2`，照 6.1 的 `fs/open.c`）：
  - `struct open_how`：长度小于 24 是 EINVAL，大于一页是 E2BIG，24 字节以后的部分不全是 0 是 E2BIG（`copy_struct_from_user`）。
  - 不是 `O_PATH` 时加上 `O_LARGEFILE`，之后按 `build_open_flags` 的顺序检查：去掉 `O_CLOEXEC` 和 `FMODE_NONOTIFY` 之后不在 `VALID_OPEN_FLAGS` 里的标志、不认识的 `resolve` 位、`BENEATH` 和 `IN_ROOT` 同时给，都是 EINVAL；要新建（`O_CREAT`、`O_TMPFILE`）时 mode 只能是权限位，不新建时 mode 必须是 0；`__O_TMPFILE` 要带 `O_DIRECTORY`、不带 `O_CREAT`、能写；`O_PATH` 只能配 `O_DIRECTORY`、`O_NOFOLLOW`、`O_CLOEXEC`。这些都先于读路径（坏的路径指针也是 EINVAL）。6.4 起 `O_DIRECTORY | O_CREAT` 也是 EINVAL，6.1 没有。
  - `RESOLVE_CACHED`：新建、截断、`O_TMPFILE` 的是 EAGAIN。6.1 拿 `O_TMPFILE` 宏比，它含 `O_DIRECTORY`，所以带 `O_DIRECTORY` 的也是 EAGAIN（后来的内核改成只看 `__O_TMPFILE`）。别的照常解析：tuxport 的解析没有"缓存里没有就要阻塞"的情形可以区分，与 gVisor 一样不管它。
  - 起点（`path_init`）：有作用域（`BENEATH`、`IN_ROOT`）时根目录就是起点；`IN_ROOT` 的绝对路径也从 dirfd 起（dirfd 无效是 EBADF）；别的绝对路径不看 dirfd，`BENEATH` 的绝对路径因此是 EXDEV 而不是 EBADF。之后与 openat 共用打开的路径（`open_path_with`）。
- **路径解析**（`vfs::walk_resolve`，照 6.1 的 `fs/namei.c`，`vfs::resolve` 是这几个位）：
  - `BENEATH`：开头的 `/`、绝对路径的符号链接（`nd_jump_root`）、在起点上的 `..`（`follow_dotdot` 的 `in_root`）是 EXDEV；`a/..` 回到起点照常。
  - `IN_ROOT`：这几种都回到起点，`..` 停在起点上。
  - `NO_SYMLINKS`：要跟随的符号链接（途中的，和不带 `O_NOFOLLOW` 时最后的）是 ELOOP，与 nosymfollow 的挂载一样（`pick_link`）。
  - 魔法链接（`nd_jump_link`）：`/proc/<pid>/fd/*`、`cwd`、`root`、`exe`、`ns/*`（`Pseudo::magic`；`exe` 在 tuxport 里按内容跟随，这里也当作魔法链接）。先查能不能看那个进程（EACCES），再看限制：`NO_MAGICLINKS` 是 ELOOP，`NO_XDEV` 时目标在别的挂载上是 EXDEV，有作用域时是 EXDEV。`/proc/self`、`thread-self` 是普通的链接。
  - `NO_XDEV`：名字上挂着东西（进到那个挂载里）、`..` 越过挂载的根或者回到挂着东西的目录，都是 EXDEV；绝对路径的符号链接要求根目录已经取过、与链接所在的目录在同一个挂载里。Linux 的 nd->root 在绝对路径、有作用域时一开始就取，否则到第一个 `..` 或第一次跳到根目录时才取，`nd_jump_root` 拿当前的挂载与还是空的 nd->root.mnt 比，所以相对路径里第一个绝对路径的符号链接总是 EXDEV（`a/../abs` 就不是）。
  - `O_CREAT` 沿着悬空的符号链接新建目标（`open_path` 的循环）时同样受限：父目录的解析带着这些位，`NO_SYMLINKS`、nosymfollow 的挂载不跟随（nosymfollow 这里原来漏了），绝对路径的目标按上面的规则。nd->root 取过没有在这里近似地按原来的路径看（绝对路径、有作用域、有 `..`，或者这个循环里已经跳过一次）。

## 测试

- 新的差分测试组 `openat2`：在 `/tmp` 里搭一棵带各种符号链接的目录树，比 `open_how` 的长度和各种不合法的标志、`RESOLVE_CACHED`、五种限制下的绝对路径、`..`、符号链接、魔法链接和跨挂载，以及 `O_CREAT` 沿着悬空链接新建，两个执行层下都与原生逐字相同。原生的 6.8 与 6.1 不同的两处（`O_DIRECTORY | O_CREAT`、`RESOLVE_CACHED` 配 `O_DIRECTORY`）不比；推送之后 CI 上更新的内核又多一处：6.1、6.8 去掉 `FMODE_NONOTIFY` 这一位、照常打开，它返回 EINVAL，也拿掉了。
- 第一版漏了 `NO_XDEV` 时 nd->root 还没取过的情形（相对路径里的绝对链接放行了），差分测试发现、按 `nd_jump_root` 改。
- bubblewrap 0.12 现在真的走 openat2，`bwrap`、`namespaces`、`tmpfs` 三组照常通过。
- 单元测试 `openat2_resolve_flags_limit_the_walk`（`vfs`）：五种限制在仓库和挂载上的各种情形，新建时的父目录。
- LTP 新通过 openat201、openat202、openat203（原来因为 ENOSYS 跳过）。

## 推迟项

- `O_CREAT` 沿着悬空的符号链接新建时，nd->root 取过没有是近似的（不看路径前面的符号链接里有没有 `..` 或绝对路径）：只影响 `NO_XDEV` 加 `O_CREAT` 加悬空的绝对链接这种组合。
- 没有在真的 GNU tar（配 glibc ≥ 2.43 的新版本）上试过：手头的发行版里都没有，差分测试按它的调用方式（`O_PATH | O_DIRECTORY`、`RESOLVE_BENEATH`）比过。→ Ubuntu 26.04 的 tar 1.35+dfsg-4ubuntu0.4 就是，解包带子目录的归档照常，openat2 的调用与原生逐字相同（M9 之前：Ubuntu 26.04 的普查）
- `O_TMPFILE` 仍不支持（与 openat 一样，打开目录时 EISDIR，程序据此退回）。
