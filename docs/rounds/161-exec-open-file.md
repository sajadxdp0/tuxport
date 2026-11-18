# M9 之前：经由 fd 执行 memfd 和已经删除的文件（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 8，也是 [/proc/<pid>/exe 指向程序文件本身](153-exe-file.md)的推迟项：

- runc 起容器之前把自己复制进 memfd、封住，再经由 `/proc/self/fd/<n>` 执行（防 CVE-2019-5736），tuxport 里是 `fork/exec /proc/self/fd/6: permission denied`；
- buildah 的 chroot 隔离（containers/storage 的 `pkg/unshare`）把自己复制进 memfd 再 fexecve，得到 `Error during reexec(...): No such file or directory`，`podman build --isolation chroot` 不能用。

153 篇判断"用到它的多是 runc、systemd 这类 D 档的程序"，容器要用，就做了。

## 做法

- 原来 execve 解析出节点之后按 inode 重新打开 object：memfd 的 object 在创建时就删了，已经删除的文件的 object 在删除提交之后也没了，所以是 ENOENT；`/proc/<pid>/fd/<n>` 跟随到的打开文件（`OpenFd`）不是仓库节点，是 EACCES。
- 现在两种"经由 fd 执行"都改用打开文件记着的宿主文件（`process::open_open_file`），与第 153 轮执行已经删除的 exe 一样：
  - execveat 的 `AT_EMPTY_PATH`（musl、glibc 的 fexecve）执行的 fd 是普通文件时；
  - 路径跟随 `/proc/<pid>/fd/<n>` 得到的是打开着的普通文件时（管道这些仍是 EACCES）。
  
  权限检查与按路径执行相同；noexec、nosuid 按打开它时经由的挂载，memfd 不经由挂载。新程序的 exe 记下这个打开文件的挂载、inode 和宿主文件，`/proc/self/exe`、maps 与 Linux 一样是 `/memfd:<名字> (deleted)`、`<路径> (deleted)`（memfd 的显示路径本来就带着后缀，记 exe 时先去掉，读的时候再按 nlink 加）。
- **memfd 不算写者**：Linux 的 memfd_create 用 `alloc_file_pseudo` 建文件，不经过 `do_dentry_open`，不增加 `i_writecount`，所以 memfd 自己的 fd 是 `O_RDWR` 也能执行；经由 `/proc/self/fd/<n>` 以写方式重新打开的照常算写者，执行是 ETXTBSY。`OpenFile` 加了 `pseudo` 标记，memfd_create 的打开文件不算进"以写方式打开着"（`open_for_write`）。

## 测试

- 差分测试 `containers` 组新增：把一个打印 `/proc/self/exe`、`AT_EXECFN` 和 maps 的小程序复制进 memfd 和普通文件，看 memfd 和删除了的文件经由几种方式执行、以写方式打开着时的 ETXTBSY、`MFD_NOEXEC_SEAL` 和不能执行的 fd 的 EACCES，两个执行层下都与原生逐字相同。不比 comm：执行 fd 时 Linux 6.8 用 fd 的编号，新内核改成了文件名。
- 在 tuxport 里实际跑：`podman build --isolation chroot` 能构建了，RUN 一步在 chroot 里跑 `apk`，构建出来的镜像能运行。

## 推迟项

- memfd 不经由挂载：Linux 的 memfd 在内核内部的 shm 挂载上，这里 noexec、nosuid 都不适用，结果一样。
- 执行之后再经由 memfd 自己的 fd 写它，已经映射的代码会不会变：Linux 上 memfd 是共享内存，私有映射的页在写时复制之前看得到；runc 先封住了，没有对照。
