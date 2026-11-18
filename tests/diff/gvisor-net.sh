# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on inet sockets, netlink and readiness: TCP sockets, bind, unbound IP
# sockets, netlink sockets and rtnetlink (links, addresses, routes, rules, multicast groups),
# /proc/net, epoll and poll. Run by tests/diff/gtest/run.sh, one process per test. Tests that
# differ for a reason recorded in a map, or that cannot be compared, are skipped, with the reason
# above them.
# Cannot be compared: Tuxport hands the malformed destination to the host socket, and in this VM
# the host's AppArmor answers EAFNOSUPPORT before Linux's own EINVAL check
SKIP="IPUnboundSockets/IPUnboundSocketTest.InsufficientBufferTOS/*"
# kernel/net D3 and the kernel/netlink map: host interfaces, addresses and routes in the initial
# namespace cannot be changed (the multicast-group tests change lo there; in a new namespace they
# would still need lo renamed, routes on lo, notifications carrying the request's seq and port, and
# sends to groups); unsupported change requests (rules) answer EOPNOTSUPP; netlink destination
# addresses are ignored (LinkMulticastGroupCapNetAdmin)
R=NetlinkRouteIpv4AndIpv6Tests/NetlinkRouteIpInvariantTest
SKIP="$SKIP:NetlinkRouteTest.AddAndRemoveAddr:NetlinkRouteTest.LinkedToNetns"
SKIP="$SKIP:NetlinkRouteTest.LinkMulticastGroup*:$R.AddAndRemoveRoute/*:$R.AddAndRemoveRule/*"
SKIP="$SKIP:$R.AddressMulticastGroup/*:$R.AddressPrefixRouteMulticastGroup/*"
SKIP="$SKIP:$R.RouteMulticastGroup/*:$R.RouteMulticastGroupSameDestination/*"
# kernel/netlink: netlink requests are not validated as Linux validates them (wrong-size attributes
# of ip route get are skipped)
SKIP="$SKIP:NetlinkRouteTest.GetRouteOversizedDestination:NetlinkRouteTest.GetRouteFamilyMismatch"
# kernel/netns: routes are unicast only and lo cannot be a next hop; route requests insist on
# exact address sizes; gateways resolve to the last matching subnet; next-hop checks miss Linux's
# special cases; IPv6 over virtual devices is unusable
SKIP="$SKIP:$R.NewDefaultRouteWithoutGateway/*:$R.DeleteDefaultRouteWithoutGateway/*"
SKIP="$SKIP:NetlinkRouteTest.DeleteIpv6RouteIgnoresType:NetlinkRouteTest.NewRouteUnsupportedType"
SKIP="$SKIP:NetlinkRouteTest.NewRouteUnspecType:NetlinkRouteTest.NewRouteTypeOutOfRange"
SKIP="$SKIP:NetlinkRouteTest.NewRouteRejectTypeWithNexthop:NetlinkRouteTest.NewRouteGatewayNotOnLink"
SKIP="$SKIP:NetlinkRouteTest.NewRouteGatewayOnOtherLink:NetlinkRouteTest.NewRouteFamilyMismatch"
SKIP="$SKIP:NetlinkRouteTest.NewRouteWithDestinationGatewayFamily"
SKIP="$SKIP:NetlinkRouteTest.NewRouteDestinationFamily"
SKIP="$SKIP:NetlinkRouteTest.NewRouteDestinationShorterThanPrefix:$R.NewRouteLowestIndex/*"
SKIP="$SKIP:NetlinkRouteTest.NewRouteGatewayOnLink:NetlinkRouteTest.NewRouteLinkLocalGateway"
SKIP="$SKIP:$R.RouteMulticastGroupReplace/*:$R.NewRouteWithoutOif/1:$R.NewRouteViaAddress/1"
# kernel/net D7: the SNMP error counters are always 0
SKIP="$SKIP:ProcNetSnmp.TcpEstab:ProcNetSnmp.TcpReset:ProcNetSnmp.UdpNoPorts"
# Cannot be compared: runs only on a kernel 7 or newer (CI's), skipping itself on 6.8 and Tuxport
SKIP="$SKIP:NetlinkRouteTest.VethAddRefusedPeerNetnsLeavesNothing"
# Cannot be compared: skips itself on a kernel without Linux 5d39580f68e6 (the dev VM's 6.8), while
# Tuxport refuses SO_ATTACH_FILTER on TCP without CAP_NET_ADMIN as 6.1.y does
SKIP="$SKIP:AllInetTests/SimpleTcpSocketTest.SetSocketAttachFilterWithoutNetAdmin/*"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" tcp_socket_test bind_test \
    socket_ip_unbound_test socket_netlink_test socket_netlink_route_test proc_net_test epoll_test \
    poll_test
