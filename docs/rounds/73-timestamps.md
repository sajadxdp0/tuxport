# M9 之前：目录和文件的时间戳（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 1：目录的 mtime、ctime 不随目录项变化，普通文件的 ctime 不随内容变化。PHP 的 5 个 stat、touch 测试因此失败；Python 进程从一个目录 import 过之后，在那里新建的模块 import 不到（importlib 按目录的 mtime 决定要不要重新列目录）；Maildir、git 的 untracked cache 这类按目录 mtime 发现变化的程序会漏掉修改。

## 做法

- 目录：在里面新建（create、mkdir、mknod、symlink、link）、删除（unlink、rmdir）、改名时，它的 mtime、ctime 改成当时的时间；改名时两边的目录都改，同一个目录只改一次。元数据缓存里换上新的行，不作废：路径解析每一级都要读目录的行。`RENAME_EXCHANGE` 另外改两个 inode 的 ctime（与 ext4 相同）。
- 普通文件的 ctime 改为取自 object：write、truncate、utimensat 由宿主改；数据库里改了它的元数据（chmod、chown、链接数、改名、xattr）时 chmod 一下 object，由宿主把 ctime 改成当时的时间。object 的权限位固定为 0600，新建时也用 0600（原来是 0666 减去 umask）。
  - 第一版取"数据库里那一列和 object 的 ctime 中较晚的一个"，差分测试里 chmod 之后 ctime 不变。原因是 virtiofs 上 object 的时间戳由 macOS 的时钟打，比虚拟机的时钟快 84 毫秒，数据库里刚写进去的 ctime 比 50 毫秒前 truncate 留在 object 上的还早。Android 上 object 在本地存储，没有这种偏差；但同一个时间戳混用两个时钟本来就不可靠，Linux 文件的时间戳取自粗粒度的时钟，和精确的时钟也会差一个 jiffy。
- utimensat 设成"现在"（times 为 NULL 或 `UTIME_NOW`）时，普通文件把 `UTIME_NOW` 原样交给宿主，和 Linux 一样由文件系统取时间。原来由内核层读 guest 的时钟算好再设：virtiofs 上 touch 之后的 mtime 比文件新建时还早，CPython 的 test_touch_common 因此一直失败（第 70 轮 test_pathlib 的 3 个失败）。只存在数据库里的 inode（目录、符号链接等），utimensat 也改 ctime，只给访问时间时也改。
- 导入 rootfs 时，目录的 mtime 放到最后再设：之后在目录里新建条目会把它改成现在。GNU tar 也是这样做的。
- object 的 ctime 是 Tier 1 翻译的"版本"（§4.2 代码段的比对）。现在 chmod、改名也会改它，版本变得更频繁，只会多核对一次，不会用错翻译。

## 测试

- 仓库的单元测试新增 `directory_and_change_times_follow_modifications`，导入的测试检查目录的 mtime 还是 tar 里的。
- 差分测试 python 组加上时间戳的检查：各种目录操作之后目录的 mtime 和 ctime，write、truncate、chmod、utimensat 之后文件的 ctime，import 找得到后来在同一个目录里新建的模块，两层都与原生相同。
- PHP 的 `ext/standard/tests/file` 全部通过，第 70 轮失败的 `lstat_stat_*`、`touch_basic` 都在其中；CPython 的 test_mailbox、test_importlib、test_shutil、test_tempfile、test_glob、test_posix、test_stat、test_zipfile、test_tarfile、test_compileall 等通过，test_pathlib 只剩 FICLONE 造成的错误（问题 6，→ 已修好（M9 之前：FICLONE 返回 EOPNOTSUPP））。test_os 的 `test_utime_current`、`test_utime_current_old` 要求 touch 之后的 mtime 和 `time.time()` 相差不到 50 毫秒，virtiofs 上 object 的时间戳快了 84 毫秒，所以失败；在虚拟机本地卷上的仓库里只差 1–3 毫秒。
- 开销：在 `tuxport-bench` 卷上，每次操作的微秒数：create 11.7–12.4 → 12.3–13.2，rename 5.8 → 8.0–8.3（其中 object 的 chmod 约 1.6，目录的时间约 0.7），unlink 3.6 → 4.3，stat 1.4–1.5 不变。剖析用的 shell、git、gcc、python、apk 负载看不出差别。

## 推迟项

- 删除之后还打开着的普通文件，fchmod、fchown、fsetxattr 不改它的 ctime：object 已经从 `objects` 目录里删掉了，按名字 chmod 不到。
- 在 virtiofs 上（开发用的 colima 虚拟机里，仓库放在 `target/` 下）文件的时间戳由 macOS 的时钟打，和 guest 的时钟差几十毫秒，拿文件时间和 `time.time()` 比的测试会失败（CPython 的 test_utime_current）。Android 上没有这个问题。
