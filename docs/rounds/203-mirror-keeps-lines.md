# M9：换镜像不丢用户加的源（2026-11-03）

[设置页](190-settings-default-alpine.md)那一轮的推迟项：设置里选镜像时把包管理器的源整个重写一遍，用户自己加的行一起没了。Alpine 上常见的是加 testing（`@testing https://…/edge/testing`），Debian 上是在组件里加 `contrib non-free`、在 suites 里加 `trixie-backports`，换一次镜像就得重新加；导入之后自动测速、在设置里选回"自动"重新测速，也是整个重写。

## 做法

不再重写整个文件，只换内置镜像的地址（`Rootfs.retarget`）：

- 内置镜像是设置里列出的那几个（官方、清华、中科大、阿里云），http、https 都算；地址里镜像的部分（Alpine 是 `dl-cdn.alpinelinux.org/alpine` 这样的主机加路径，Debian 只是主机名）换成选的那个，协议照旧（用户装了 ca-certificates 之后把 apt 改成了 https 的，换镜像之后还是 https）。后面跟的分支、仓库、`/debian-security` 都不动。
- 别的地址不动：用户自己的仓库、第三方的源（比如 Docker 的 `https://download.docker.com/linux/debian`）。不按路径的样子（`…/alpine/v3.22/main`、以 `/debian` 结尾）去认：第三方的仓库也常用同样的布局，按主机认更可预料。
- 注释掉的行也照换，取消注释之后就是新镜像。
- 源文件不在了就照导入时那样重写。
- 源里一处内置镜像也没有（用户已经手动换成了列表以外的镜像）时不动文件，也不改记下的镜像；设置里选的时候弹出提示"源里没有列表中的镜像，没有改动，请手动修改。"（Android 12 起 toast 最多显示两行，写上源文件的路径就被截断了），选"自动"测速之后也一样。

## 测试

仪器测试 `manualMirrorIsWritten` 扩充：源里有用户加的行、注释掉的行、第三方仓库时只换内置镜像的地址、协议照旧，全是列表以外的镜像时什么也不改，源文件不在时重写；Debian 的 deb822 内容直接调 `retarget` 测了同样几种情形。在 Pixel 4a（API 34）的模拟器上通过；也在模拟器上手动试了源里全是列表以外的镜像时选镜像，弹出提示，什么也没改。

## 推迟项

- 只改 App 写的那一个文件（`/etc/apk/repositories`、`/etc/apt/sources.list.d/debian.sources`）。用户在 `sources.list.d` 里另加的文件、`/etc/apt/sources.list` 里的地址仍是原来的镜像。
- 列表以外的镜像仍要手改，设置里不能填自己的地址。
- `security.debian.org` 不算内置镜像（App 写的是 `deb.debian.org/debian-security`），用户改成它之后换镜像时那一行不跟着换。
