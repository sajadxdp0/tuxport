# M9 之前：NETLINK_GENERIC 只有 nlctrl（2026-10-29）

[没有实现的 netlink 协议建不了](83-netlink-protocols.md)那一轮的推迟项：`NETLINK_GENERIC` 在 Linux 上总是有的（genetlink 编在内核里，不能去掉），tuxport 却是 EPROTONOSUPPORT。用它的程序（iw、ethtool 的 netlink 接口、wg、iotop 的 TASKSTATS）先建 socket、再按名字查自己要的族，查不到才按"内核没有这个子系统"处理；socket 本身建不了，有的程序不预期。

## 做法

- **只有 nlctrl 一个族**（`core/kernel/src/net.rs`）：这是 Linux 上总会有的那个族（`GENL_ID_CTRL`，编号 16），别的子系统一个也没有，相当于没有编进 nl80211、ethtool、wireguard、TASKSTATS 的 Linux。行为照着原生的 Linux 6.8（差分测试的宿主）做，逐字节相同：
  - `CTRL_CMD_GETFAMILY` 按 `CTRL_ATTR_FAMILY_ID` 或 `CTRL_ATTR_FAMILY_NAME` 查：nlctrl 回答 `CTRL_CMD_NEWFAMILY`（名字、编号、版本 2、头长 0、属性上限 0、两个命令和它们的标志、组播组 `notify`）；别的名字是 ENOENT，两个属性都没有是 EINVAL。6.2 起 nlctrl 的命令各有自己的策略，族的属性上限是 0，6.1 上是 10，按差分测试的宿主取 0。
  - 转储（`NLM_F_DUMP`）只列出 nlctrl，`NLMSG_DONE` 跟在同一个数据报里，之后不另外确认。
  - 发往别的族编号是 ENOENT，nlctrl 不认识的命令是 EOPNOTSUPP。`CTRL_CMD_GETPOLICY` 只能转储：要的族不存在是 ENOENT，不说哪个族是 EINVAL；nlctrl 自己的策略没有做，是 EOPNOTSUPP。
  - 错误消息附上整条请求（没有设 `NETLINK_CAP_ACK` 时 Linux 就这样）；确认只附上消息头，带 `NLM_F_CAPPED`。回答和确认各是一个数据报（`genlmsg_reply`、`netlink_ack`）：第一版放在了同一个数据报里，程序读一次就把确认一起读走、丢掉了，再等确认时一直等下去，差分测试的程序因此卡住。
- **netlink socket 的 `SO_RCVTIMEO`**：原来只记在 TCP、UDP 的 socket 上，netlink socket 没有回答可读时一直等，不看它。上面那次卡住也是因为这个：程序设了 2 秒的超时，Linux 上会返回 EAGAIN。现在按它超时，`getsockopt` 读得回来。

## 测试

- 差分测试 compat 组新增一个 C 程序：nlctrl 的各种查询、错误、转储和 `SO_RCVTIMEO` 到期，打印回答的每个属性、错误码、确认和错误消息，与原生逐行相同。转储只看 nlctrl，别的族取决于内核配置。
- 单元测试新增一个：同样几种请求，回答和确认是两个数据报，错误消息附上整条请求。
- guest 程序 `net.c` 改成检查 `NETLINK_GENERIC` 建得了；`waits.c` 加上在 netlink socket 上按 `SO_RCVTIMEO` 等一次。

## 推迟项

- nlctrl 自己的策略（`CTRL_CMD_GETPOLICY`）没有做，是 EOPNOTSUPP；iproute2 的 `genl ctrl policy` 用得到，别的程序只查自己的族的策略，那些族都不存在。
- 没有别的族。要模拟某个子系统（比如让 ethtool 走 netlink）时再加。
- 没有扩展确认：设了 `NETLINK_EXT_ACK` 的 socket 在 Linux 上的错误消息带着说明文字、出错的属性等（`NLMSGERR_ATTR_*`），这里没有。→ rtnetlink 改设备的请求出错时带说明文字（只给 6.1 里就有的那几条，M9 之前：有状态的网卡），nftables 的带出错属性的偏移（M9 之前：nftables 的子集和 socket 层的 DNAT）；generic netlink 和 rtnetlink 别的请求仍没有
- `NETLINK_ROUTE` 的错误消息仍只附上请求的消息头，确认也不带 `NLM_F_CAPPED`（[M4](06-m4.md) 起）；Linux 与上面 genetlink 的一样。→ 已补上（M9 之前：路由表），两种协议用同一个函数回答确认和错误
