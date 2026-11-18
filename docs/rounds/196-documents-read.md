# M9：在"文件"App 里浏览（只读）（2026-11-02）

设计文档 §11.3 的 v1：用 DocumentsProvider 把登录账号的家目录放进系统的"文件"App（DocumentsUI），设置里可以再把整个 `/` 以只读方式放进去。用户在"文件"App 里用"复制到…"把文件拿出来，或者在别的 App 的文件选择框里直接选 tuxport 里的文件。M9 计划把它分成两轮，这一轮只做读，下一轮做写（复制进来、新建、删除、改名）。

## 做法

### 不经过内核，直接读仓库（`vfs::browse`）

DocumentsProvider 随时会被调用：用户打开"文件"App、别的 App 打开文件选择框时，App 的进程被拉起来，内核多半没在运行，为了列个目录启动内核太重。所以 provider 直接在仓库里按 guest 路径查（内核层新的 `vfs::browse`，经由 `tuxport_kernel::browse` 每次另外打开一次仓库）：

- **查找**：按组件走，符号链接跟随（绝对路径从 guest 的根算起，相对路径从链接所在的目录算起），`..` 是目录真正的上级，在根上不动；最多跟随 40 次（Linux 的 `MAXSYMLINKS`），再多是 ELOOP。
- **只给目录和普通文件**：设备节点、FIFO、socket 不列、打不开（宿主读 FIFO 会一直等下去），断了的符号链接不列。符号链接显示成它指向的东西（`/usr/bin` 下面那些显示成 busybox 的大小）。
- **读文件**：普通文件的内容本来就是宿主上的 object 文件，以只读打开它，fd 交给 `ParcelFileDescriptor.adoptFd`。不复制、不经过管道，"文件"App 和别的 App 拿到的就是这个文件。普通文件的大小、修改时间也是取自 object（§8.1），总是最新的。
- **看到的是仓库里的样子**：挂载都不管，`/proc`、`/sys`、`/dev`、`/run` 是仓库里的空目录或者开机前的内容，tmpfs、bind 挂载里的看不到。内核运行着时也是这样。
- **不检查 guest 的权限**：仓库里的一切本来就属于这台设备的用户，在终端里 `doas` 也都看得到。
- **与运行中的内核共用仓库**：仓库支持别的连接同时打开（§8.1）：另外打开时，运行中的内核先提交推迟的写事务，所以 provider 看得到刚在终端里写的文件；这个连接开着期间内核不再推迟提交，所以 provider 每次调用用完就关，不长期开着。另一个方向：内核打开仓库时要是唯一的用户，才做开机的清理（清空 `/dev/shm`、`/run`，删掉上次没来得及提交的 object）。provider 在 binder 线程上，可能正好在内核启动时开着仓库，于是加了一把读写锁（`Rootfs.access`）：provider 每次调用持读锁，启动内核、删掉仓库（换发行版、重置）持写锁。

### 文档 ID 和根

- **文档 ID 就是 guest 的绝对路径**，比如 `/home/alice/notes/hello.txt`，调试时一眼看得懂。guest 的文件名是任意字节，合法 UTF-8 的部分照原样，`%`、控制字符和不是 UTF-8 的字节写成 `%XX`（`browse::encode_path`，能原样换回来；JNI 那边一项一行、制表符分隔，这样也不会被名字里的制表符、换行弄乱）。显示的名字是解码之后的，不是 UTF-8 的字节显示成 �。
- **根**：
  - **家目录**：设置里登录账号（[第 193 轮](193-login-account.md)）的家目录，规则与 `login.sh` 一样：自动是最先建的普通用户，没有时是 root。标题是 tuxport，说明是"root · Alpine Linux 3.24"这样的账号和发行版，可用空间是 App 数据所在分区的（`StorageManager.getAllocatableBytes`，算上系统能清掉的缓存）。家目录不在时不列这个根。支持搜索和"是不是子文档"的判断（树的授权要用）。
  - **整个系统**：设置里新加的开关"在'文件'App 里显示整个系统"，默认关。打开之后多一个根 `/`，说明是"整个系统（只读）"。
  - 换了登录账号、开关、内核起来之后（仓库可能是刚导入的）通知"文件"App 重新读根。
- **只认规范的、在根里面的 ID**：ID 要以 `/` 开头，不能有空的、`.`、`..` 组件，不然当作不存在。没打开整个系统时只认家目录里面的：以前打开时授予别的 App 的 `/` 的权限，关掉之后就用不了了，换了登录账号之后原来的家目录也一样。树的授权由框架调用 `isChildDocument` 检查，`/root/dt/../../etc/motd` 这样的 ID 在规范性检查里就不过。
- **只读**：文档没有可写、可删、可改名的标志，"文件"App 里只有"打开方式""复制到…""详细信息"；以写的模式打开抛出 `UnsupportedOperationException`。
- **类型**：目录是 `vnd.android.document/directory`，文件按扩展名查 `MimeTypeMap`，查不到的是 `application/octet-stream`。
- **搜索**：在根下面按名字找（不分大小写），广度优先，浅的在前，最多 200 个，最多看 50,000 项；不进符号链接指向的目录，免得绕圈。
- 登录账号的家目录要读 `/etc/passwd`（又是一次打开仓库），一次浏览要查好几次，记 5 秒。

Manifest 里的 provider 由 `MANAGE_DOCUMENTS` 权限保护（只有系统的"文件"App 能直接调用），`grantUriPermissions`，别的 App 经由用户在文件选择框里选的授权访问。

## 测试

- 内核层（`cargo test`）新增 `paths_resolve_through_links`（各种符号链接和路径写法、断了的链接、ELOOP、FIFO、EISDIR、ENOTDIR）、`listings_follow_links_and_hide_special_files`（列目录、搜索）、`paths_survive_the_round_trip`（各种名字编码之后能原样换回来）。
- 仪器测试新增 `documentsShowTheHomeDirectory`：经由 `ContentResolver` 看运行中的内核刚写的文件，测了根、列目录、读文件、家目录外面和不规范的 ID、树的授权、搜索、打开整个系统之后的根，在 Pixel 4a（API 34）的模拟器上通过。第一版测试搜索时只给了 URI 里的查询，框架要求参数里也有（`QUERY_ARG_DISPLAY_NAME`，"文件"App 是这样传的），照它改了测试。
- 手动（模拟器上的"文件"App）：家目录的根、菜单里只有只读的几项、"复制到…"下载目录之后内容正确；打开"整个系统"之后有两个根，`/usr/bin` 里上百个链接一秒内列出来。

## 推迟项

- 写（复制进来、新建、删除、改名、以写的模式打开）：下一轮。→ 已做，只在家目录里面（[M9：在"文件"App 里改家目录](197-documents-write.md)）
- 看到的是仓库，不是运行中的 guest 看到的：tmpfs（`/run`、`/dev/shm`）、bind 挂载、容器的 overlay 里的文件看不到。
- guest 里改了文件，"文件"App 不会自动刷新，要重新进这个目录（内核没有把变化告诉 provider）。→ 新建、删除、改名会刷新了：内核记下目录项改过的目录，服务定时取走、通知"文件"App（[M9："文件"App 与 guest 双向的通知](201-documents-notify.md)）；只改了文件内容（大小、时间变了）的仍不刷新
- 符号链接都跟随，家目录里指向外面的链接也是：拿到家目录树的授权的 App，经由这样的链接能读到家目录外面的文件（链接是 guest 里的用户自己建的）。
- 没有扩展名的文本文件（`/etc/profile`、脚本）是 `application/octet-stream`，"文件"App 里点开没有 App 能打开，只能复制出去。没有按内容猜类型。→ 按扩展名猜不出的看开头 4 KB，文本给 `text/plain`（[M9："文件"App 按内容认出文本文件](204-text-by-content.md)）
- 没有缩略图：图片只显示图标。
- 搜索只按名字，不支持"文件"App 的按类型、按时间过滤（根上没有声明 `COLUMN_QUERY_ARGS`）。
- `Rootfs.access` 只管 provider 和启动内核、删掉仓库；App 里别处另外打开仓库的（设置页列账号、开机后读 `boot.log`）没有持锁，原来就是这样，它们都不在内核启动的同时发生。
