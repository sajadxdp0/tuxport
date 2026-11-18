# M9：发布时放出的源码（2026-11-03）

[许可证](192-license.md)那一轮记下的发布前义务里，能先做的是把要随 APK 放出的源码整理出来。App 按 GPLv3/AGPLv3 要提供整个 App 的对应源码（包括静态链接进 `libtuxport.so` 的 crate），APK 里 Alpine minirootfs 的 GPL 包要提供它们的源码；开源许可页已经写了"GPL 包的源码与 tuxport 的源码一起放在 https://github.com/sajadxdp0/tuxport"。这一轮写了生成这些文件的脚本。

## 做法

`scripts/release-sources [输出目录]`（默认 `target/release-sources`）生成三份：

| 文件 | 内容 |
|---|---|
| `tuxport-<提交>.tar.gz` | `git archive HEAD`；工作区有没提交的改动时报错，免得放出的与构建的不是同一份 |
| `tuxport-<提交>-vendor.tar.gz` | `cargo vendor --locked`：所有 crate 的源码 |
| `alpine-<版本>-gpl-sources.tar` | Alpine minirootfs 里 GPL 包的源码，每个 origin 一个目录，加上一份 README |

Alpine 那一份：

- 包从哪里来：`scripts/fetch-rootfs` 下载（验签名、比 sha256）的 tarball 里的 `lib/apk/db/installed`，取每个包的版本、许可证、origin 和 aports 提交（`c:`），与开源许可页列的是同一份。许可证里有 GPL 的按 origin 去重：3.24.3 是 alpine-baselayout、apk-tools、busybox、musl（musl-utils 有 GPL-2.0-or-later 的部分）、pax-utils（scanelf），共 9 个包。
- aports 的目录：GitLab 的 archive 接口可以按提交只取一个子目录（`…/-/archive/<提交>/aports-<提交>.tar.gz?path=main/<origin>`），不用克隆整个 aports。
- 上游源码：没有执行 APKBUILD（`source=` 里有 `$pkgver` 这些变量，求值等于运行下载来的 shell 代码）。APKBUILD 的 `sha512sums=` 一节列着每个源文件最终的文件名和 SHA-512，目录里没有的就是上游下载的，Alpine 的 distfiles 镜像按文件名存着它们（`https://distfiles.alpinelinux.org/distfiles/v3.24/<文件>`，没有时再看 `edge/`）。下载之后、连同目录里的补丁和配置文件，都按 sha512sums 校验，有一个对不上就报错退出。

在开发机（macOS）上跑一次 26 秒：tuxport 源码 2.3 MB，vendor 30 MB，Alpine 的 4.9 MB（busybox 75 个文件，musl 12 个，alpine-baselayout 14 个，apk-tools、pax-utils 各是上游的一个 tarball）。

## 测试

- 跑通一遍，看了生成的内容：每个目录里有 APKBUILD、补丁和上游的 tarball，README 列出 9 个包。
- 故意把 pax-utils tarball 的期望摘要改错：报 "sha512 mismatch for pax-utils-1.3.9.tar.xz"，退出码 1，不生成 Alpine 那一份。
- 只加了脚本和文档，没有动代码，没有重跑测试。

## 推迟项

- 仓库仍是私有的：发布之前公开，还是随 APK 附上提供源码的书面承诺，由用户定；三份文件放在哪里（GitHub 的 release 附件）也要那时定。
- Android 库（AndroidX、Kotlin 标准库，Apache-2.0）的源码没有打包，它们的源码由上游公开发布（Maven 上的 `-sources.jar`）；vendored 的 Termux 终端库在 tuxport 的源码里。
- 脚本只认 3.24 这样的版本号去找 distfiles 的目录；换到 edge 或者以后 distfiles 改了布局要跟着改。
- 设置里下载的 Debian 不经 tuxport 分发，不在其列（[许可证](192-license.md)那一轮的推迟项）。
