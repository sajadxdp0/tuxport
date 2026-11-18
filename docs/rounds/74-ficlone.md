# M9 之前：FICLONE 返回 EOPNOTSUPP（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 6：FICLONE 返回 ENOTTY。Python 3.14 的 `pathlib.Path.copy()` 先用 FICLONE 试 reflink，只在 EBADF、EOPNOTSUPP、ETXTBSY、EXDEV 时退回普通的复制，遇到 ENOTTY 就报错，所以 tuxport 上它复制任何文件都失败（test_pathlib 的 28 个错误）。

## 做法

FICLONE、FICLONERANGE 在看文件类型之前处理，与 Linux 的 `do_vfs_ioctl` 相同，按 `ioctl_file_clone` 和 `generic_file_rw_checks` 的顺序检查：源 fd 无效是 EBADF；有一边不是仓库里的文件（管道、socket、设备、`/proc` 的文件）是 EXDEV，Linux 上它们不在同一个挂载点；有一边是目录是 EISDIR；源不可读、目标不可写或者带 `O_APPEND` 是 EBADF。都通过之后返回 EOPNOTSUPP，和宿主上的 ext4、f2fs 一样不支持 reflink，程序退回普通的复制（`copy_file_range` 本来就交给宿主）。FIDEDUPERANGE 检查头里的保留项（EINVAL）和源的类型，之后同样是 EOPNOTSUPP。

- 没有把 FICLONE 转给宿主：Android 的文件系统都不支持 reflink，转过去也是 EOPNOTSUPP。

## 测试

- 差分测试 python 组加了 FICLONE、FICLONERANGE 各种情况的错误码和 `Path.copy()` 的结果，两层都与原生相同（原生的 overlayfs 底下是 ext4，同样是 EOPNOTSUPP）。
- CPython 的 test_pathlib、test_shutil 全部通过。LTP 的 ioctl_ficlone 系列要块设备来格式化各种文件系统，用不上。

## 推迟项

- 宿主的文件系统支持 reflink 时（btrfs、XFS 上的开发机）也不转给宿主。
- `O_PATH` 打开的源 fd 在 Linux 上是 EBADF（`fdget` 不接受），这里是 EXDEV。→ 已补上（M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口），ioctl 的 fd 本身是 `O_PATH` 打开的也是 EBADF
