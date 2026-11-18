//! Network namespace sysctls (`/proc/sys/net`): the interface settings `ipv4/conf/<interface>`
//! and `ipv6/conf/<interface>` (including `all` and `default`), plus `ipv4/ip_forward`,
//! `ipv4/ip_local_port_range`, `ipv4/ip_unprivileged_port_start` (binding a port below it needs
//! `CAP_NET_BIND_SERVICE`), `ipv4/ping_group_range` (kernel group IDs, shown and written in the
//! caller's user namespace, but only recorded: ping sockets are handed to the host, and Android
//! allows all groups) and `core/somaxconn`.
//!
//! Entries and defaults as in Linux 6.1 (`devinet_sysctl_table`, `ipv4_devconf_dflt`;
//! `addrconf_sysctl`, `ipv6_devconf_dflt`). All values are integers (except `stable_secret`,
//! which reads as EIO until it is set), and writes are parsed as by `proc_dointvec`. Only a few
//! take effect (handled in [`crate::netdev`]): `disable_ipv6` removes and restores an interface's
//! IPv6, `forwarding` written on `all` (`ip_forward` is its alias) is applied to every interface,
//! and IPv4 `default` is applied to the interfaces that have not set that entry on their own. The
//! others are only recorded: Tuxport does no forwarding and sends or receives no ARP or router
//! advertisements.
//!
//! As on Linux, a new interface copies the `default` of its namespace; a new namespace's IPv4 part
//! copies the initial namespace's current values, and its IPv6 part is the defaults (as when
//! `devconf_inherit_init_net` is 0). An interface moved to another namespace starts over from that
//! namespace's `default`.

use std::collections::BTreeMap;

use crate::cred::Cred;
use crate::errno::Errno;

/// One interface setting: name, default value (of `default` and of interfaces), value of `all`.
type Item = (&'static str, i64, i64);

/// The IPv4 entries (sorted by name). `all` and `default` differ only in `accept_source_route`.
pub(crate) const V4: [Item; 33] = [
    ("accept_local", 0, 0),
    ("accept_redirects", 1, 1),
    ("accept_source_route", 1, 0),
    ("arp_accept", 0, 0),
    ("arp_announce", 0, 0),
    ("arp_evict_nocarrier", 1, 1),
    ("arp_filter", 0, 0),
    ("arp_ignore", 0, 0),
    ("arp_notify", 0, 0),
    ("bc_forwarding", 0, 0),
    ("bootp_relay", 0, 0),
    ("disable_policy", 0, 0),
    ("disable_xfrm", 0, 0),
    ("drop_gratuitous_arp", 0, 0),
    ("drop_unicast_in_l2_multicast", 0, 0),
    ("force_igmp_version", 0, 0),
    ("forwarding", 0, 0),
    ("igmpv2_unsolicited_report_interval", 10000, 10000),
    ("igmpv3_unsolicited_report_interval", 1000, 1000),
    ("ignore_routes_with_linkdown", 0, 0),
    ("log_martians", 0, 0),
    ("mc_forwarding", 0, 0),
    ("medium_id", 0, 0),
    ("promote_secondaries", 0, 0),
    ("proxy_arp", 0, 0),
    ("proxy_arp_pvlan", 0, 0),
    ("route_localnet", 0, 0),
    ("rp_filter", 0, 0),
    ("secure_redirects", 1, 1),
    ("send_redirects", 1, 1),
    ("shared_media", 1, 1),
    ("src_valid_mark", 0, 0),
    ("tag", 0, 0),
];

/// The IPv6 entries (sorted by name). `all` and `default` differ only in `accept_dad`; on an
/// interface `mtu` is the interface's MTU ([`Conf::mtu6`]), and `stable_secret` is kept
/// separately.
pub(crate) const V6: [Item; 58] = [
    ("accept_dad", 1, 0),
    ("accept_ra", 1, 1),
    ("accept_ra_defrtr", 1, 1),
    ("accept_ra_from_local", 0, 0),
    ("accept_ra_min_hop_limit", 1, 1),
    ("accept_ra_mtu", 1, 1),
    ("accept_ra_pinfo", 1, 1),
    ("accept_ra_rt_info_max_plen", 0, 0),
    ("accept_ra_rt_info_min_plen", 0, 0),
    ("accept_ra_rtr_pref", 1, 1),
    ("accept_redirects", 1, 1),
    ("accept_source_route", 0, 0),
    ("accept_untracked_na", 0, 0),
    ("addr_gen_mode", 0, 0),
    ("autoconf", 1, 1),
    ("dad_transmits", 1, 1),
    ("disable_ipv6", 0, 0),
    ("disable_policy", 0, 0),
    ("drop_unicast_in_l2_multicast", 0, 0),
    ("drop_unsolicited_na", 0, 0),
    ("enhanced_dad", 1, 1),
    ("force_mld_version", 0, 0),
    ("force_tllao", 0, 0),
    ("forwarding", 0, 0),
    ("hop_limit", 64, 64),
    ("ignore_routes_with_linkdown", 0, 0),
    ("ioam6_enabled", 0, 0),
    ("ioam6_id", 65535, 65535),
    ("ioam6_id_wide", 4_294_967_295, 4_294_967_295),
    ("keep_addr_on_down", 0, 0),
    ("max_addresses", 16, 16),
    ("max_desync_factor", 600, 600),
    ("mc_forwarding", 0, 0),
    ("mldv1_unsolicited_report_interval", 10000, 10000),
    ("mldv2_unsolicited_report_interval", 1000, 1000),
    ("mtu", 1280, 1280),
    ("ndisc_evict_nocarrier", 1, 1),
    ("ndisc_notify", 0, 0),
    ("ndisc_tclass", 0, 0),
    ("optimistic_dad", 0, 0),
    ("proxy_ndp", 0, 0),
    ("ra_defrtr_metric", 1024, 1024),
    ("regen_max_retry", 3, 3),
    ("router_probe_interval", 60, 60),
    ("router_solicitation_delay", 1, 1),
    ("router_solicitation_interval", 4, 4),
    ("router_solicitation_max_interval", 3600, 3600),
    ("router_solicitations", -1, -1),
    ("rpl_seg_enabled", 0, 0),
    ("seg6_enabled", 0, 0),
    ("seg6_require_hmac", 0, 0),
    ("stable_secret", 0, 0),
    ("suppress_frag_ndisc", 1, 1),
    ("temp_prefered_lft", 86400, 86400),
    ("temp_valid_lft", 604_800, 604_800),
    ("use_oif_addrs_only", 0, 0),
    ("use_optimistic", 0, 0),
    ("use_tempaddr", 0, 0),
];

/// The minimum IPv6 MTU.
pub(crate) const IPV6_MIN_MTU: i64 = 1280;

/// One set of interface settings (of one family): the value of every entry, and which were set
/// individually (IPv4 `default` only changes those not set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conf {
    pub vals: Vec<i64>,
    pub set: Vec<bool>,
    /// IPv6 `stable_secret`: reads as EIO until it is set.
    pub secret: Option<[u8; 16]>,
    /// IPv6 `mtu`: the value set; on an interface, the interface's MTU while not set.
    pub mtu6: Option<i64>,
}

impl Conf {
    /// Defaults: those of `all` or of `default` (interfaces).
    pub fn new(v6: bool, all: bool) -> Conf {
        let t = table(v6);
        Conf {
            vals: t.iter().map(|&(_, d, a)| if all { a } else { d }).collect(),
            set: vec![false; t.len()],
            secret: None,
            mtu6: None,
        }
    }

    /// What a new interface copies: the `default` values, all counted as not set.
    pub fn inherit(&self) -> Conf {
        Conf {
            set: vec![false; self.set.len()],
            mtu6: None,
            ..self.clone()
        }
    }

    pub fn get(&self, v6: bool, name: &str) -> i64 {
        position(v6, name.as_bytes()).map_or(0, |i| self.vals[i])
    }
}

/// The sysctls of one network namespace.
#[derive(Debug, Clone)]
pub(crate) struct NetConf {
    pub all4: Conf,
    pub dflt4: Conf,
    pub all6: Conf,
    pub dflt6: Conf,
    /// Settings of interfaces that are not virtual devices (lo in a new namespace, the host's
    /// interfaces in the initial namespace), by interface index; `default`'s when absent. Those of
    /// virtual devices are kept on the device.
    pub other: BTreeMap<u32, (Conf, Conf)>,
    /// `core/somaxconn`.
    pub somaxconn: i64,
    /// `ipv4/ip_local_port_range`.
    pub port_range: (i64, i64),
    /// `ipv4/ip_unprivileged_port_start` (Linux's `sysctl_ip_prot_sock`).
    pub prot_sock: i64,
    /// `ipv4/ping_group_range`.
    pub ping_group: (i64, i64),
    /// Written [`SYSCTLS`] (those never written have their default values).
    pub sysctls: BTreeMap<&'static str, String>,
}

impl Default for NetConf {
    fn default() -> NetConf {
        NetConf {
            all4: Conf::new(false, true),
            dflt4: Conf::new(false, false),
            all6: Conf::new(true, true),
            dflt6: Conf::new(true, false),
            other: BTreeMap::new(),
            somaxconn: 4096,
            port_range: (32768, 60999),
            prot_sock: 1024,
            // The initial namespace allows all groups, as Android (and systemd's default
            // configuration) does; a new namespace gets Linux's default, which allows nobody
            // (only recorded, not enforced)
            ping_group: (0, 2_147_483_647),
            sysctls: BTreeMap::new(),
        }
    }
}

impl NetConf {
    /// For a new namespace: IPv4 copies `init`'s current `all` and `default`; the rest are
    /// defaults.
    pub fn for_new_ns(init: &NetConf) -> NetConf {
        NetConf {
            all4: init.all4.inherit(),
            dflt4: init.dflt4.inherit(),
            ping_group: (1, 0),
            ..NetConf::default()
        }
    }

    /// Settings of interface `index`, which is not a virtual device (copying `default` when
    /// absent).
    pub fn other_mut(&mut self, index: u32) -> &mut (Conf, Conf) {
        let (d4, d6) = (self.dflt4.inherit(), self.dflt6.inherit());
        self.other.entry(index).or_insert((d4, d6))
    }
}

/// Position of entry `name` in the table.
pub(crate) fn position(v6: bool, name: &[u8]) -> Option<usize> {
    table(v6).iter().position(|(n, ..)| n.as_bytes() == name)
}

/// The table of one family.
pub(crate) fn table(v6: bool) -> &'static [Item] {
    if v6 { &V6 } else { &V4 }
}

/// Read-only entries (`mc_forwarding`).
pub(crate) fn read_only(name: &[u8]) -> bool {
    name == b"mc_forwarding"
}

/// The values an entry may take: mostly any int; a few have ranges (those Linux handles with
/// `proc_dointvec_minmax` and `proc_douintvec`). The upper bound of `mtu` is the interface's MTU,
/// checked in [`crate::netdev`].
pub(crate) fn check(v6: bool, name: &str, v: i64) -> Result<(), Errno> {
    let (lo, hi) = match (v6, name) {
        (true, "addr_gen_mode") => (0, 3),
        (true, "ioam6_id") => (0, 65535),
        (true, "ioam6_id_wide") => (0, u32::MAX as i64),
        (true, "accept_untracked_na") => (0, 2),
        (true, "ndisc_tclass") => (0, 255),
        (true, "ra_defrtr_metric") => (1, u32::MAX as i64),
        (true, "mtu") => (IPV6_MIN_MTU, i32::MAX as i64),
        _ => (i32::MIN as i64, i32::MAX as i64),
    };
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// Reading `stable_secret`: EIO until it is set.
pub(crate) fn show_secret(s: Option<[u8; 16]>) -> Result<String, Errno> {
    let s = s.ok_or(Errno::EIO)?;
    Ok(format!("{}\n", std::net::Ipv6Addr::from(s)))
}

/// Writing `stable_secret`: an IPv6 address.
pub(crate) fn parse_secret(data: &[u8]) -> Result<[u8; 16], Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    s.trim()
        .parse::<std::net::Ipv6Addr>()
        .map(|a| a.octets())
        .map_err(|_| Errno::EINVAL)
}

/// Writes `ipv4/ip_local_port_range`: two numbers, both between 1 and 65535, the first not
/// greater than the second, and not below `ip_unprivileged_port_start` (`prot_sock`); otherwise
/// EINVAL (Linux 6.1's `ipv4_local_port_range`).
pub(crate) fn parse_port_range(data: &[u8], prot_sock: i64) -> Result<(i64, i64), Errno> {
    let (lo, hi) = parse_pair(data)?;
    if !(1..=65535).contains(&lo) || !(1..=65535).contains(&hi) || lo > hi || lo < prot_sock {
        return Err(Errno::EINVAL);
    }
    Ok((lo, hi))
}

/// Writes `ipv4/ip_unprivileged_port_start`: 0 to 65535, not greater than the lower bound of the
/// local port range (`ipv4_privileged_ports`).
pub(crate) fn parse_prot_sock(data: &[u8], range_low: i64) -> Result<i64, Errno> {
    let v = parse_int(data)?;
    if !(0..=65535).contains(&v) || range_low < v {
        return Err(Errno::EINVAL);
    }
    Ok(v)
}

/// Writes `ipv4/ping_group_range`: two group IDs of `cred`'s user namespace, stored as kernel
/// IDs (`ipv4_ping_group_range`); one not mapped there is EINVAL, and when the second is less
/// than the first, they become 1, 0, which allows nobody.
pub(crate) fn parse_ping_group(data: &[u8], cred: &Cred) -> Result<(i64, i64), Errno> {
    let (lo, hi) = parse_pair(data)?;
    let gid = 0..=i64::from(u32::MAX - 1);
    if !gid.contains(&lo) || !gid.contains(&hi) {
        return Err(Errno::EINVAL);
    }
    let (Some(klo), Some(khi)) = (cred.gid_in(lo as u32), cred.gid_in(hi as u32)) else {
        return Err(Errno::EINVAL);
    };
    Ok(if hi < lo || khi < klo {
        (1, 0)
    } else {
        (klo.into(), khi.into())
    })
}

/// Parses a written integer as `proc_dointvec` does: leading whitespace and a sign are allowed,
/// and whitespace (a newline) may follow.
pub(crate) fn parse_int(data: &[u8]) -> Result<i64, Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let s = s.trim_start();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    if !s[end..].trim().is_empty() {
        return Err(Errno::EINVAL);
    }
    s[..end].parse().map_err(|_| Errno::EINVAL)
}

/// Two integers (writes of `ip_local_port_range` and `ping_group_range`).
fn parse_pair(data: &[u8]) -> Result<(i64, i64), Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let mut it = s.split_ascii_whitespace().map(|w| w.parse::<i64>());
    let (Some(Ok(a)), Some(Ok(b)), None) = (it.next(), it.next(), it.next()) else {
        return Err(Errno::EINVAL);
    };
    Ok((a, b))
}

/// Other sysctls under `/proc/sys/net` (docs/rounds/177-net-sysctls.md): the path (under `core/`,
/// `ipv4/`, `ipv6/`, `unix/`), the default value (tabs between several numbers), the permission
/// bits, and whether it exists only in the initial namespace. These are the ones Linux 6.1
/// provides in a new network namespace (except subdirectories such as ipv4/conf and route).
/// Written values are only recorded and have no effect.
pub(crate) const SYSCTLS: [(&str, &str, u16, bool); 145] = [
    ("core/netdev_max_backlog", "1000", 0o644, true),
    ("core/optmem_max", "131072", 0o644, false),
    ("core/rmem_default", "212992", 0o644, true),
    ("core/rmem_max", "212992", 0o644, true),
    ("core/txrehash", "1", 0o644, false),
    ("core/wmem_default", "212992", 0o644, true),
    ("core/wmem_max", "212992", 0o644, true),
    ("core/xfrm_acq_expires", "30", 0o644, false),
    ("core/xfrm_aevent_etime", "10", 0o644, false),
    ("core/xfrm_aevent_rseqth", "2", 0o644, false),
    ("core/xfrm_larval_drop", "1", 0o644, false),
    ("ipv4/fib_multipath_hash_fields", "7", 0o644, false),
    ("ipv4/fib_multipath_hash_policy", "0", 0o644, false),
    ("ipv4/fib_multipath_use_neigh", "0", 0o644, false),
    ("ipv4/fib_notify_on_flag_change", "0", 0o644, false),
    ("ipv4/fwmark_reflect", "0", 0o644, false),
    ("ipv4/icmp_echo_enable_probe", "0", 0o644, false),
    ("ipv4/icmp_echo_ignore_all", "0", 0o644, false),
    ("ipv4/icmp_echo_ignore_broadcasts", "1", 0o644, false),
    ("ipv4/icmp_errors_use_inbound_ifaddr", "0", 0o644, false),
    ("ipv4/icmp_ignore_bogus_error_responses", "1", 0o644, false),
    ("ipv4/icmp_ratelimit", "1000", 0o644, false),
    ("ipv4/icmp_ratemask", "6168", 0o644, false),
    ("ipv4/igmp_link_local_mcast_reports", "1", 0o644, false),
    ("ipv4/igmp_max_memberships", "20", 0o644, false),
    ("ipv4/igmp_max_msf", "10", 0o644, false),
    ("ipv4/igmp_qrv", "2", 0o644, false),
    ("ipv4/ip_autobind_reuse", "0", 0o644, false),
    ("ipv4/ip_default_ttl", "64", 0o644, false),
    ("ipv4/ip_dynaddr", "0", 0o644, false),
    ("ipv4/ip_early_demux", "1", 0o644, false),
    ("ipv4/ip_forward_update_priority", "1", 0o644, false),
    ("ipv4/ip_forward_use_pmtu", "0", 0o644, false),
    ("ipv4/ip_local_reserved_ports", "", 0o644, false),
    ("ipv4/ip_no_pmtu_disc", "0", 0o644, false),
    ("ipv4/ip_nonlocal_bind", "0", 0o644, false),
    ("ipv4/ipfrag_high_thresh", "4194304", 0o644, false),
    ("ipv4/ipfrag_low_thresh", "3145728", 0o644, false),
    ("ipv4/ipfrag_max_dist", "64", 0o644, false),
    ("ipv4/ipfrag_time", "30", 0o644, false),
    ("ipv4/nexthop_compat_mode", "1", 0o644, false),
    ("ipv4/raw_l3mdev_accept", "1", 0o644, false),
    ("ipv4/tcp_abort_on_overflow", "0", 0o644, false),
    ("ipv4/tcp_adv_win_scale", "1", 0o644, false),
    (
        "ipv4/tcp_allowed_congestion_control",
        "reno cubic",
        0o444,
        false,
    ),
    ("ipv4/tcp_app_win", "31", 0o644, false),
    ("ipv4/tcp_autocorking", "1", 0o644, false),
    (
        "ipv4/tcp_available_congestion_control",
        "reno cubic",
        0o444,
        false,
    ),
    ("ipv4/tcp_base_mss", "1024", 0o644, false),
    ("ipv4/tcp_challenge_ack_limit", "2147483647", 0o644, false),
    ("ipv4/tcp_child_ehash_entries", "0", 0o644, false),
    ("ipv4/tcp_comp_sack_delay_ns", "1000000", 0o644, false),
    ("ipv4/tcp_comp_sack_nr", "44", 0o644, false),
    ("ipv4/tcp_comp_sack_slack_ns", "100000", 0o644, false),
    ("ipv4/tcp_congestion_control", "cubic", 0o644, false),
    ("ipv4/tcp_dsack", "1", 0o644, false),
    ("ipv4/tcp_early_demux", "1", 0o644, false),
    ("ipv4/tcp_early_retrans", "3", 0o644, false),
    ("ipv4/tcp_ecn", "2", 0o644, false),
    ("ipv4/tcp_ecn_fallback", "1", 0o644, false),
    // Sized by memory (memory_default); these are the values for 4 GB of memory
    ("ipv4/tcp_ehash_entries", "-32768", 0o444, false),
    ("ipv4/tcp_fack", "0", 0o644, false),
    ("ipv4/tcp_fastopen", "1", 0o644, false),
    ("ipv4/tcp_fastopen_blackhole_timeout_sec", "0", 0o644, false),
    (
        "ipv4/tcp_fastopen_key",
        "00000000-00000000-00000000-00000000",
        0o600,
        false,
    ),
    ("ipv4/tcp_fin_timeout", "60", 0o644, false),
    ("ipv4/tcp_frto", "2", 0o644, false),
    ("ipv4/tcp_fwmark_accept", "0", 0o644, false),
    ("ipv4/tcp_invalid_ratelimit", "500", 0o644, false),
    ("ipv4/tcp_keepalive_intvl", "75", 0o644, false),
    ("ipv4/tcp_keepalive_probes", "9", 0o644, false),
    ("ipv4/tcp_keepalive_time", "7200", 0o644, false),
    ("ipv4/tcp_l3mdev_accept", "0", 0o644, false),
    ("ipv4/tcp_limit_output_bytes", "1048576", 0o644, false),
    ("ipv4/tcp_max_reordering", "300", 0o644, false),
    // These two are sized by memory too
    ("ipv4/tcp_max_syn_backlog", "256", 0o644, false),
    ("ipv4/tcp_max_tw_buckets", "16384", 0o644, false),
    ("ipv4/tcp_migrate_req", "0", 0o644, false),
    ("ipv4/tcp_min_rtt_wlen", "300", 0o644, false),
    ("ipv4/tcp_min_snd_mss", "48", 0o644, false),
    ("ipv4/tcp_min_tso_segs", "2", 0o644, false),
    ("ipv4/tcp_moderate_rcvbuf", "1", 0o644, false),
    ("ipv4/tcp_mtu_probe_floor", "48", 0o644, false),
    ("ipv4/tcp_mtu_probing", "0", 0o644, false),
    ("ipv4/tcp_no_metrics_save", "0", 0o644, false),
    ("ipv4/tcp_no_ssthresh_metrics_save", "1", 0o644, false),
    ("ipv4/tcp_notsent_lowat", "4294967295", 0o644, false),
    ("ipv4/tcp_orphan_retries", "0", 0o644, false),
    ("ipv4/tcp_pacing_ca_ratio", "120", 0o644, false),
    ("ipv4/tcp_pacing_ss_ratio", "200", 0o644, false),
    ("ipv4/tcp_pingpong_thresh", "1", 0o644, false),
    ("ipv4/tcp_probe_interval", "600", 0o644, false),
    ("ipv4/tcp_probe_threshold", "8", 0o644, false),
    ("ipv4/tcp_recovery", "1", 0o644, false),
    ("ipv4/tcp_reflect_tos", "0", 0o644, false),
    ("ipv4/tcp_reordering", "3", 0o644, false),
    ("ipv4/tcp_retrans_collapse", "1", 0o644, false),
    ("ipv4/tcp_retries1", "3", 0o644, false),
    ("ipv4/tcp_retries2", "15", 0o644, false),
    ("ipv4/tcp_rfc1337", "0", 0o644, false),
    ("ipv4/tcp_rmem", "4096\t131072\t6291456", 0o644, false),
    ("ipv4/tcp_sack", "1", 0o644, false),
    ("ipv4/tcp_slow_start_after_idle", "1", 0o644, false),
    ("ipv4/tcp_stdurg", "0", 0o644, false),
    ("ipv4/tcp_syn_retries", "6", 0o644, false),
    ("ipv4/tcp_synack_retries", "5", 0o644, false),
    ("ipv4/tcp_syncookies", "1", 0o644, false),
    ("ipv4/tcp_thin_linear_timeouts", "0", 0o644, false),
    ("ipv4/tcp_timestamps", "1", 0o644, false),
    ("ipv4/tcp_tso_rtt_log", "9", 0o644, false),
    ("ipv4/tcp_tso_win_divisor", "3", 0o644, false),
    ("ipv4/tcp_tw_reuse", "2", 0o644, false),
    ("ipv4/tcp_window_scaling", "1", 0o644, false),
    ("ipv4/tcp_wmem", "4096\t16384\t4194304", 0o644, false),
    ("ipv4/tcp_workaround_signed_windows", "0", 0o644, false),
    ("ipv4/udp_early_demux", "1", 0o644, false),
    ("ipv4/udp_l3mdev_accept", "0", 0o644, false),
    ("ipv4/udp_rmem_min", "4096", 0o644, false),
    ("ipv4/udp_wmem_min", "4096", 0o644, false),
    ("ipv4/xfrm4_gc_thresh", "32768", 0o644, false),
    ("ipv6/anycast_src_echo_reply", "0", 0o644, false),
    ("ipv6/auto_flowlabels", "1", 0o644, false),
    ("ipv6/bindv6only", "0", 0o644, false),
    ("ipv6/fib_multipath_hash_fields", "7", 0o644, false),
    ("ipv6/fib_multipath_hash_policy", "0", 0o644, false),
    ("ipv6/fib_notify_on_flag_change", "0", 0o644, false),
    ("ipv6/flowlabel_consistency", "1", 0o644, false),
    ("ipv6/flowlabel_reflect", "0", 0o644, false),
    ("ipv6/flowlabel_state_ranges", "0", 0o644, false),
    ("ipv6/fwmark_reflect", "0", 0o644, false),
    ("ipv6/idgen_delay", "1", 0o644, false),
    ("ipv6/idgen_retries", "3", 0o644, false),
    ("ipv6/ioam6_id", "16777215", 0o644, false),
    ("ipv6/ioam6_id_wide", "72057594037927935", 0o644, false),
    ("ipv6/ip6frag_high_thresh", "4194304", 0o644, false),
    ("ipv6/ip6frag_low_thresh", "3145728", 0o644, false),
    ("ipv6/ip6frag_time", "60", 0o644, false),
    ("ipv6/ip_nonlocal_bind", "0", 0o644, false),
    ("ipv6/max_dst_opts_length", "2147483647", 0o644, false),
    ("ipv6/max_dst_opts_number", "8", 0o644, false),
    ("ipv6/max_hbh_length", "2147483647", 0o644, false),
    ("ipv6/max_hbh_opts_number", "8", 0o644, false),
    ("ipv6/seg6_flowlabel", "0", 0o644, false),
    ("ipv6/xfrm6_gc_thresh", "32768", 0o644, false),
    ("unix/max_dgram_qlen", "10", 0o644, false),
];

/// The [`SYSCTLS`] entry of path `path` under `/proc/sys/net/` (such as
/// `ipv4/tcp_keepalive_time`); `init` means the initial namespace (entries that exist only there
/// count too).
pub(crate) fn sysctl(
    path: &[u8],
    init: bool,
) -> Option<&'static (&'static str, &'static str, u16, bool)> {
    SYSCTLS
        .iter()
        .find(|(p, _, _, only)| p.as_bytes() == path && (init || !only))
}

/// Reads an entry of [`SYSCTLS`]: the written value or the default.
pub(crate) fn sysctl_read(conf: &NetConf, path: &'static str) -> String {
    if let Some(v) = conf.sysctls.get(path) {
        return format!("{v}\n");
    }
    let dflt = memory_default(path, tuxport_hal::memory_info().0);
    let dflt = dflt
        .as_deref()
        .unwrap_or_else(|| sysctl(path.as_bytes(), true).map_or("", |e| e.1));
    format!("{dflt}\n")
}

/// Defaults sized by memory: with `cnt` buckets in the TCP connection table ([`ehash_entries`]),
/// in a new namespace `tcp_ehash_entries` is -cnt (sharing one table with the initial
/// namespace), `tcp_max_tw_buckets` is cnt / 2, and `tcp_max_syn_backlog` is cnt / 128, at least
/// 128 (`tcp_sk_init`). `total` is the total memory (bytes); 0 means unknown (the table's values
/// for 4 GB are used).
fn memory_default(path: &str, total: u64) -> Option<String> {
    let cnt = ehash_entries(total)?;
    match path {
        "ipv4/tcp_ehash_entries" => Some(format!("-{cnt}")),
        "ipv4/tcp_max_tw_buckets" => Some((cnt / 2).to_string()),
        "ipv4/tcp_max_syn_backlog" => Some((cnt / 128).max(128).to_string()),
        _ => None,
    }
}

/// How many buckets Linux's TCP connection table has (`tcp_init` calls
/// `alloc_large_system_hash`): memory rounded up to whole MB, one per 128 KB, taken to a power of
/// two, at most 512 K. 32768 with 4 GB of memory, 65536 with 8 GB.
fn ehash_entries(total: u64) -> Option<u64> {
    if total == 0 {
        return None;
    }
    let mb = total.div_ceil(1 << 20);
    Some(((mb << 20) >> 17).next_power_of_two().min(512 * 1024))
}

/// Writes an entry of [`SYSCTLS`]: integer entries have as many numbers as the default; writing
/// some changes that many leading ones, and extra numbers are ignored (`proc_dointvec`);
/// a congestion control algorithm must exist (otherwise ENOENT); reserved ports and TCP Fast Open
/// keys must follow their formats.
pub(crate) fn sysctl_write(
    conf: &mut NetConf,
    path: &'static str,
    data: &[u8],
) -> Result<(), Errno> {
    let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let text = text.trim_end_matches(['\n', '\0']).trim();
    let value = match path {
        "ipv4/tcp_congestion_control" => {
            if !matches!(text, "reno" | "cubic") {
                return Err(Errno::ENOENT);
            }
            text.to_owned()
        }
        "ipv4/ip_local_reserved_ports" => {
            let ok = text
                .split(',')
                .all(|r| r.is_empty() || r.split('-').all(|p| p.trim().parse::<u16>().is_ok()));
            if !ok {
                return Err(Errno::EINVAL);
            }
            text.to_owned()
        }
        "ipv4/tcp_fastopen_key" => {
            let ok = text.split(',').all(|k| {
                let parts: Vec<&str> = k.split('-').collect();
                parts.len() == 4
                    && parts
                        .iter()
                        .all(|p| p.len() <= 8 && u32::from_str_radix(p, 16).is_ok())
            });
            if !ok {
                return Err(Errno::EINVAL);
            }
            text.to_owned()
        }
        _ => {
            let old = sysctl_read(conf, path);
            let mut vals: Vec<String> = old.trim_end().split('\t').map(str::to_owned).collect();
            let new: Vec<&str> = text.split_whitespace().collect();
            // Extra numbers are ignored (`__do_proc_dointvec` reads as many as the table has)
            let new = &new[..new.len().min(vals.len())];
            if new.is_empty() || new.iter().any(|v| v.parse::<i64>().is_err()) {
                return Err(Errno::EINVAL);
            }
            for (slot, v) in vals.iter_mut().zip(new) {
                *slot = v.parse::<i64>().unwrap().to_string();
            }
            vals.join("\t")
        }
    };
    conf.sysctls.insert(path, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Other sysctls: defaults; integer writes change as many leading values as given and ignore
    /// extra ones, non-numbers are EINVAL, a congestion control algorithm must exist, and some
    /// entries exist only in the initial namespace.
    #[test]
    fn plain_sysctls() {
        let mut c = NetConf::default();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_keepalive_time"), "7200\n");
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "4096\t131072\t6291456\n");
        sysctl_write(&mut c, "ipv4/tcp_rmem", b"8192\n").unwrap();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "8192\t131072\t6291456\n");
        sysctl_write(&mut c, "ipv4/tcp_rmem", b"1 2 3 4").unwrap();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "1\t2\t3\n");
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_fin_timeout", b"abc"),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_fin_timeout", b""),
            Err(Errno::EINVAL)
        );
        sysctl_write(&mut c, "ipv4/tcp_congestion_control", b"reno\n").unwrap();
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_congestion_control", b"bbr"),
            Err(Errno::ENOENT)
        );
        assert_eq!(sysctl_read(&c, "ipv4/tcp_congestion_control"), "reno\n");
        sysctl_write(&mut c, "ipv4/ip_local_reserved_ports", b"8080,9000-9010").unwrap();
        assert_eq!(
            sysctl_write(&mut c, "ipv4/ip_local_reserved_ports", b"x"),
            Err(Errno::EINVAL)
        );
        // A new namespace lacks the entries that exist only in the initial namespace
        assert!(sysctl(b"core/rmem_max", true).is_some());
        assert!(sysctl(b"core/rmem_max", false).is_none());
        assert!(sysctl(b"ipv4/tcp_keepalive_time", false).is_some());
        assert!(sysctl(b"ipv4/no_such", true).is_none());
        // Every namespace has its own copy
        let other = NetConf::for_new_ns(&c);
        assert_eq!(
            sysctl_read(&other, "ipv4/tcp_congestion_control"),
            "cubic\n"
        );
    }

    #[test]
    fn tables_are_sorted_and_parse_like_proc_dointvec() {
        for t in [&V4[..], &V6[..]] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
        assert_eq!(parse_int(b"1\n"), Ok(1));
        assert_eq!(parse_int(b"  -3 \n"), Ok(-3));
        assert_eq!(parse_int(b"+7"), Ok(7));
        assert_eq!(parse_int(b"x"), Err(Errno::EINVAL));
        assert_eq!(parse_int(b"1 2"), Err(Errno::EINVAL));
        assert_eq!(parse_int(b""), Err(Errno::EINVAL));
        assert_eq!(check(true, "addr_gen_mode", 4), Err(Errno::EINVAL));
        assert_eq!(check(false, "forwarding", 2), Ok(()));
        let c = Conf::new(false, false);
        assert_eq!(c.get(false, "accept_source_route"), 1);
        assert_eq!(Conf::new(false, true).get(false, "accept_source_route"), 0);
        assert_eq!(parse_port_range(b"1024\t65535\n", 1024), Ok((1024, 65535)));
        assert_eq!(parse_port_range(b"80 65534", 1024), Err(Errno::EINVAL));
        assert_eq!(parse_port_range(b"80 65534", 0), Ok((80, 65534)));
        assert_eq!(parse_port_range(b"2000 1001", 0), Err(Errno::EINVAL));
        assert_eq!(parse_prot_sock(b"0", 32768), Ok(0));
        assert_eq!(parse_prot_sock(b"40000", 32768), Err(Errno::EINVAL));
        let root = Cred::root();
        assert_eq!(
            parse_ping_group(b"0 2147483647", &root),
            Ok((0, 2_147_483_647))
        );
        assert_eq!(parse_ping_group(b"5 2", &root), Ok((1, 0)));
        assert_eq!(show_secret(None), Err(Errno::EIO));
    }

    /// The memory-sized entries: the native values on colima's 4 GB and 8 GB VMs, 16 GB, and the
    /// cap.
    #[test]
    fn memory_sized_defaults() {
        let vm4 = 3_999_216 * 1024;
        let vm8 = 8_307_478_528;
        assert_eq!(ehash_entries(vm4), Some(32768));
        assert_eq!(ehash_entries(vm8), Some(65536));
        assert_eq!(ehash_entries(16 << 30), Some(131072));
        assert_eq!(ehash_entries(1 << 40), Some(512 * 1024));
        assert_eq!(ehash_entries(0), None);
        let d = |p, t| memory_default(p, t).unwrap();
        assert_eq!(d("ipv4/tcp_ehash_entries", vm8), "-65536");
        assert_eq!(d("ipv4/tcp_max_syn_backlog", vm8), "512");
        assert_eq!(d("ipv4/tcp_max_syn_backlog", 1 << 30), "128");
        assert_eq!(d("ipv4/tcp_max_tw_buckets", vm4), "16384");
        assert_eq!(memory_default("ipv4/tcp_fin_timeout", vm4), None);
    }
}
