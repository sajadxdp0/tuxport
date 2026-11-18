# M9 之前：短的符号链接不占块（2026-11-01）

[Ubuntu 26.04 的普查](149-ubuntu26-survey.md)的问题 2：`ls -l` 一个有符号链接的目录，total 比原生多 4。tuxport 对所有文件按大小向上取整到 4 KB 报 `st_blocks`，符号链接也是；Linux 上短的符号链接存在 inode 里，不占数据块。

## 做法

- `encode_stat`：符号链接的目标短于这个长度时 `st_blocks` 是 0：仓库（对应 ext4）是 60 字节（`ext4_symlink`：连同结尾的 NUL 放得进 60 字节的 `i_data` 就是快速符号链接），tmpfs 这类别的是 128 字节（`shmem_symlink` 的 `SHORT_SYMLINK_LEN`）。长的照旧按一页算，与 [tmpfs 的用量](131-tmpfs-usage.md)里"长的符号链接一页"一致。

## 测试

- 差分测试 `compat2` 组：`/tmp`（原生是 ext4 上的 overlayfs）里目标 59、60 字节的符号链接的大小和块数（0、8）；`tmpfs` 组：挂上的 tmpfs 里目标 127、128 字节的（0、8）。两个执行层下都与原生相同。
