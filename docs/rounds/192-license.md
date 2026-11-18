# M9：许可证改为 AGPL-3.0-or-later，开源许可页（2026-11-02）

M9 计划里的许可证审计（风险 R3）。做之前用户提出想把 tuxport 的许可证从 MIT 改成 GPLv2 或 AGPLv3，先查了一遍现状再定。

## 现状

- **作者**：仓库是私有的，没有 fork；261 个提交的作者都是用户本人（其中 258 个带 Claude 的 Co-Authored-By）。没有别人的代码要征求同意，改许可证没有历史包袱。
- **自己的代码里没有第三方的**：扫了版权、SPDX、许可证字样，只有 Gradle wrapper（Apache-2.0，不进 APK）。基准程序、lockstep 的测试程序都是自己写的。
- **Rust 依赖**：47 个 `MIT OR Apache-2.0`，7 个 MIT，其余是 Zlib、`Unlicense OR MIT`、`0BSD OR MIT OR Apache-2.0`、`(MIT OR Apache-2.0) AND Unicode-3.0` 等，全都能选 MIT。SQLite（libsqlite3-sys 打包的）是公有领域。
- **APK 里的 Android 库**：AndroidX、Kotlin 标准库、kotlinx.coroutines、listenablefuture、JetBrains annotations、JSpecify，都是 Apache-2.0。
- **vendored 的 Termux 终端库**：termux-app 的 LICENSE.md 写的是"GPLv3 only"，例外只提到这两个库用了 jackpal 的 Terminal Emulator for Android 的代码（Apache 2.0）。按 termux-app 的历史逐个文件查：
  - 26 个 Java 文件里，`TerminalSessionClient`、`Logger`、`AndroidUtils`、文本选择的三个、sixel 的三个（`TerminalSixel`、`TerminalBitmap`、`ITermImage`）、`PopupWindowCompatGingerbread` 是 2016 年以后 Termux 的贡献者新写的。
  - 其余来自 2015 年 Fredrik Fornwall 最初的导入，之后也都被别的贡献者改过（`TerminalView.java` 有 26 次来自 agnostic-apollo 的修改）。
  - 这些贡献是在"GPLv3 only"的仓库里做的，只能按 GPLv3 对待。也就是说，App 发布时本来就得按 GPLv3 提供整个 App 的源码，tuxport 自己的代码也在内。许可证的选择实际影响的是 App 以外对核心的使用（`tuxport-run`、嵌进别的产品、拿去做服务端的沙箱）。
- **APK 里的 Alpine minirootfs**：16 个包，busybox、apk-tools、alpine-baselayout、scanelf 是 GPL-2.0-only，musl-utils 有一部分是 GPL-2.0-or-later，其余是 MIT、Apache-2.0（OpenSSL）、MPL-2.0（证书）、Zlib。它与 tuxport 只是放在一起（mere aggregation），不受 tuxport 的许可证影响，但发布 APK 时要提供这些 GPL 程序的源码。

## 选择

- **GPLv2（only）不行**：GPLv2-only 与 GPLv3-only 的 Termux 代码不能结合；按 FSF 的看法 Apache-2.0 与 GPLv2 也不兼容，APK 里的 jackpal 代码、AndroidX、Kotlin 标准库都是 Apache-2.0，换掉 Termux 的库也解决不了。"GPLv2 或更新的版本"可以，但合到 App 里实际按 GPLv3。GPLv2 唯一的好处是能直接搬 Linux 内核、QEMU 里 GPLv2-only 的代码，那又会和 App 冲突。
- **AGPLv3 兼容现有的一切**：能选 MIT 的 crate、Apache-2.0 的库都能进 AGPLv3 的作品；GPLv3 与 AGPLv3 各自的第 13 条明确允许两者结合（结合的作品里 Termux 的部分仍按 GPLv3，网络条款作用于整个作品）。R3 也就没有了。比 GPLv3 多的网络条款正好管到一个现实的场景：有人改了内核层和引擎，做成在线的代码执行沙箱（AI agent 运行代码那种服务），要公开改动。
- 代价（告诉了用户）：不少公司禁止使用 AGPL 的代码；以后想另外卖商业授权，需要版权都在自己手里，有外部贡献者时从一开始就要签 CLA；如果将来做 iOS，App Store 的条款与 GPL 系的许可证冲突（自己的代码可以加例外）。
- 还提醒了一点：copyleft 靠版权执行，主要由 AI 生成、没有人类创作成分的部分可能不受版权保护（美国版权局 2025 年的报告），会削弱任何许可证的约束力，对 copyleft 影响更大。这不是法律意见。

用户选了 **AGPL-3.0-or-later**。

## 做法

- **许可证本身**：`LICENSE` 换成 AGPL-3.0 的全文（gnu.org 的原文）；`Cargo.toml` 的 `license` 改为 `AGPL-3.0-or-later`；README 的 License 一节写上通常的声明，并说明 App 里还有哪些别的许可证的代码。
- **法律声明**（AGPL 第 0、5 条要求交互界面显示版权、不提供担保、许可证和看全文的地方）：
  - App：设置页新加"开源许可"一行，说明文字就是这段声明，点进去是开源许可页。
  - `tuxport-run --version`：照 GNU 程序的写法，在版本号后面印版权、"License AGPLv3+"、可以自由修改和分发、不提供担保。
- **开源许可页的内容在构建时生成**（`NoticesTask`，`android/app/build.gradle.kts`），放在 assets 的 `notices/`，一共 42 项：
  - tuxport：声明加 AGPL 全文。
  - Termux 的两个库：来源的提交、termux-app 的 LICENSE.md 原文（`android/licenses/termux-app-LICENSE.md`），以及 jackpal 原项目的 NOTICE（`android/licenses/jackpal-NOTICE.txt`，Apache-2.0 第 4(d) 条要求附上）。
  - Alpine minirootfs：从 tarball 里的 `lib/apk/db/installed` 读出每个包的版本、许可证和 aports 的提交（`c:` 字段，源码就在那个提交的 `main/<origin>` 里），说明源码在哪里。tar 的读取是自己写的几十行（ustar 头）。
  - Android 库：取变体运行时的依赖图（只解析图、不下载），列出 `组:名字:版本`。不在已知是 Apache-2.0 的组里时构建失败，新加库时必须先看许可证。
  - Rust crate：`cargo metadata --filter-platform aarch64-linux-android`，从 tuxport-android 出发只走普通依赖；构建依赖（cc）、过程宏（thiserror-impl、jni-sys-macros）和只有它们用到的（syn、quote、proc-macro2、unicode-ident）不在库里，不列。每个 crate 附上它发布时带的许可证文件原文（MIT 要求保留版权声明）；许可证里 MIT 是可选的一项时只附 MIT 的文件（加上 AND 的那一项，比如 Unicode），按 MIT 使用。现在是 33 个。
  - SQLite：版本从打包的 `sqlite3.h` 读，正文是它放弃版权的说明和那段祝福。
  - 最后是 GPL 3.0、GPL 2.0、Apache 2.0、MPL 2.0 的全文（`android/licenses/`，各自官网的原文）。
  - 自己写的说明按 72 列折行，与许可证原文的排法一样。
- **开源许可页**（`LicensesActivity`）：列表，点一项看全文；全文用等宽字，字号取 74 列正好放得下屏幕宽度的大小（最大 13sp），更长的行可以横向滚动。返回键从全文回到列表：Android 13 起用 `OnBackInvokedCallback`（target 37 时系统不再调用 `onBackPressed`），之前的版本仍走 `onBackPressed`（lint 的 `GestureBackNavigation` 标了 suppress 并写明原因）。
- **`scripts/fetch-rootfs` 验签名**（原来 [M5](07-m5.md) 的推迟项）：下载之后先用 Alpine 的发布密钥（`scripts/alpine-ncopa.asc`，Natanael Copa，指纹 `0482 D840 22F5 2DF1 C4E7 CD43 293A CD09 07D9 495A`）验 `.asc`，再比固定的 sha256。用 `gpgv` 配一个只含这把公钥的临时 keyring，不碰用户的 keyring，也不需要 gpg-agent。摘要对不上而签名有效时印出新的摘要，提示升级时改成它；没装 gpg 时跳过验签（摘要是固定的）。App 运行时仍只比 sha256：摘要写在 App 里，验签不会更可信。
- **设计文档**：R3 标为已解决；§15 里否决 QEMU、Unicorn 的理由从"和 MIT 冲突"改成和 AGPLv3、App 里的 GPLv3/Apache-2.0 代码冲突（它们是 GPLv2）；新增 §13.4"许可证"。

## 测试

- 仪器测试新增 `noticesCoverTheApp`：索引和文件都在，tuxport、Termux、Alpine、Android 库、Rust crate、SQLite 各项的关键内容都有，只有构建时用到的 crate 没有列进去。在 Pixel 4a（API 34）的模拟器上通过。
- 手动看了设置页的声明、开源许可页的列表和全文、返回键。
- `scripts/fetch-rootfs`：正常下载、摘要对不上、没有 gpg、tarball 改了一个字节几种情形都对。
- `tuxport-run --version` 印出声明；没有测试依赖原来的 `--version` 输出。

## 推迟项

- 发布之前要做的（都是 GPLv3/AGPLv3、GPLv2 的义务，记在这里免得漏掉）：
  - 仓库现在是私有的。随 APK 要提供整个 App 的完整源码（包括静态链接的 crate：可以在发布时附上 `cargo vendor` 的结果），所以发布之前要公开仓库，或者附上提供源码的书面承诺。开源许可页里写的源码地址是 `https://github.com/sajadxdp0/tuxport`。→ `scripts/release-sources` 生成源码和 `cargo vendor` 的包（[发布时放出的源码](200-release-sources.md)）；仓库仍是私有的，公开还是附书面承诺由用户定
  - APK 里 Alpine minirootfs 的 GPL 包（busybox、apk-tools、alpine-baselayout、scanelf、musl-utils）：要把它们在各自 aports 提交下的目录和上游源码打包，随 tuxport 的源码一起放出（开源许可页里已经这样写了）。下载、打包的脚本还没写：APKBUILD 的 `source=` 里有变量，直接执行 APKBUILD 又等于运行下载来的代码，要另想办法（比如用 Alpine 自己的 `abuild fetch` 在容器里做）。→ `scripts/release-sources` 做了（[发布时放出的源码](200-release-sources.md)）：不执行 APKBUILD，上游源码按 sha512sums 里的文件名从 Alpine 的 distfiles 镜像下载、校验
- 源文件没有加 SPDX 的许可证标识，只靠 `LICENSE` 和 `Cargo.toml`。
- 设置里下载的 Debian 由用户从 Docker 官方镜像的来源下载，不经过 tuxport 分发，没有列进开源许可页。
- 开源许可页只有英文（许可证原文本来就是英文），列表的标题、说明也没有翻译。
