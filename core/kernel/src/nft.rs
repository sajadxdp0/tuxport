//! A subset of nftables (docs/rounds/172-nftables.md): tables, chains, rules and sets over
//! `NETLINK_NETFILTER`, and the `nft_compat` that iptables-nft uses (revision queries of xt
//! extensions).
//!
//! One per network namespace ([`crate::nsproxy::NetNs::nft`]). Follows Linux's
//! `nf_tables_api.c`; the format of answers and the error codes were checked against a 6.8
//! kernel:
//!
//! - Changes all come in batches (`NFNL_MSG_BATCH_BEGIN` to `NFNL_MSG_BATCH_END`, in one
//!   datagram): a batch is committed only if all of it succeeds, and then the generation number
//!   (`NFT_MSG_GETGEN`, the `res_id` of every answer) goes up by one; if any message fails, the
//!   whole batch is discarded and the failed messages are reported (with the offset of the
//!   offending attribute when `NETLINK_EXT_ACK` is set). Needs `CAP_NET_ADMIN` in the
//!   namespace's owner (queries too, `nfnetlink_rcv`).
//! - Rule expressions are recorded as given and returned as given in dumps (iptables' `-C`
//!   compares dumps); only lookups referring to anonymous sets and jumps by id are rewritten to
//!   names (`__set0`). Table handles come from a counter in the namespace, and those of chains,
//!   rules and sets from a counter in the table; a chain's USE is the number of jumps referring
//!   to it plus the number of rules in it, and a table's USE is the number of its chains and
//!   sets.
//! - Execution ([`Nft::nat`], [`Nft::filter`]; when and where is decided in [`crate::netns`]):
//!   connect and sendto run the nat-type chains on OUTPUT, then on the PREROUTING of a namespace
//!   the connection enters (IPv4), and DNAT replaces the destination address in the socket layer
//!   (the built-in DNS rules docker writes into containers, podman's port forwarding); filter
//!   chains run on OUTPUT and along the path on PREROUTING, INPUT and FORWARD (IPv4). POSTROUTING
//!   and arp, bridge and netdev tables are only recorded, not executed; unknown expressions count
//!   as the rule not matching. `limit`, `quota` and xt's `limit` keep their state with the rule;
//!   a packet's length is that of its headers, as `meta len` gives it.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::net::{attr, message};
use crate::nsproxy::NetNs;

const NFNL_SUBSYS_CTNETLINK: u16 = 1;
const NFNL_SUBSYS_CTNETLINK_EXP: u16 = 2;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_SUBSYS_NFT_COMPAT: u16 = 11;
const NFNL_MSG_BATCH_BEGIN: u16 = 16;
const NFNL_MSG_BATCH_END: u16 = 17;
const NFNL_BATCH_GENID: u16 = 1;

const NEWTABLE: u8 = 0;
const GETTABLE: u8 = 1;
const DELTABLE: u8 = 2;
const NEWCHAIN: u8 = 3;
const GETCHAIN: u8 = 4;
const DELCHAIN: u8 = 5;
const NEWRULE: u8 = 6;
const GETRULE: u8 = 7;
const DELRULE: u8 = 8;
const NEWSET: u8 = 9;
const GETSET: u8 = 10;
const DELSET: u8 = 11;
const NEWSETELEM: u8 = 12;
const GETSETELEM: u8 = 13;
const DELSETELEM: u8 = 14;
const NEWGEN: u8 = 15;
const GETGEN: u8 = 16;
const NEWOBJ: u8 = 18;
const GETOBJ: u8 = 19;
const NEWFLOWTABLE: u8 = 22;
const GETFLOWTABLE: u8 = 23;
/// `NFT_MSG_DESTROYTABLE` to `NFT_MSG_DESTROYSETELEM` (since 6.3; table, chain, rule, set and
/// element in that order): like DEL, but also succeeding when the object does not exist.
const DESTROYTABLE: u8 = 26;
const DESTROYSETELEM: u8 = 30;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_APPEND: u16 = 0x800;
const NLM_F_NONREC: u16 = 0x100;
/// ERESTART: the generation number given at the start of the batch is out of date
/// (`NFNL_BATCH_GENID`).
const ERESTART: i32 = 85;

// Attributes
const TABLE_NAME: u16 = 1;
const TABLE_FLAGS: u16 = 2;
const TABLE_USE: u16 = 3;
const TABLE_HANDLE: u16 = 4;
const TABLE_USERDATA: u16 = 6;
const CHAIN_TABLE: u16 = 1;
const CHAIN_HANDLE: u16 = 2;
const CHAIN_NAME: u16 = 3;
const CHAIN_HOOK: u16 = 4;
const CHAIN_POLICY: u16 = 5;
const CHAIN_USE: u16 = 6;
const CHAIN_TYPE: u16 = 7;
const CHAIN_COUNTERS: u16 = 8;
const CHAIN_FLAGS: u16 = 10;
const CHAIN_ID: u16 = 11;
const CHAIN_USERDATA: u16 = 12;
const HOOK_HOOKNUM: u16 = 1;
const HOOK_PRIORITY: u16 = 2;
const RULE_TABLE: u16 = 1;
const RULE_CHAIN: u16 = 2;
const RULE_HANDLE: u16 = 3;
const RULE_EXPRESSIONS: u16 = 4;
const RULE_POSITION: u16 = 6;
const RULE_USERDATA: u16 = 7;
const RULE_CHAIN_ID: u16 = 11;
const SET_TABLE: u16 = 1;
const SET_NAME: u16 = 2;
const SET_FLAGS: u16 = 3;
const SET_KEY_TYPE: u16 = 4;
const SET_KEY_LEN: u16 = 5;
const SET_DATA_TYPE: u16 = 6;
const SET_DATA_LEN: u16 = 7;
const SET_POLICY: u16 = 8;
const SET_DESC: u16 = 9;
const SET_ID: u16 = 10;
const SET_TIMEOUT: u16 = 11;
const SET_GC_INTERVAL: u16 = 12;
const SET_USERDATA: u16 = 13;
const SET_OBJ_TYPE: u16 = 15;
const SET_HANDLE: u16 = 16;
const SET_EXPR: u16 = 17;
const SET_EXPRESSIONS: u16 = 18;
const SET_ELEM_LIST_TABLE: u16 = 1;
const SET_ELEM_LIST_SET: u16 = 2;
const SET_ELEM_LIST_ELEMENTS: u16 = 3;
const SET_ELEM_LIST_SET_ID: u16 = 4;
const SET_ELEM_KEY: u16 = 1;
const SET_ELEM_FLAGS: u16 = 3;
const SET_ELEM_KEY_END: u16 = 10;
const GEN_ID: u16 = 1;
const GEN_PROC_PID: u16 = 2;
const GEN_PROC_NAME: u16 = 3;
const LIST_ELEM: u16 = 1;
const EXPR_NAME: u16 = 1;
const EXPR_DATA: u16 = 2;
const DATA_VALUE: u16 = 1;
const DATA_VERDICT: u16 = 2;
const VERDICT_CODE: u16 = 1;
const VERDICT_CHAIN: u16 = 2;
const VERDICT_CHAIN_ID: u16 = 3;
const IMMEDIATE_DREG: u16 = 1;
const IMMEDIATE_DATA: u16 = 2;
const LOOKUP_SET: u16 = 1;
const LOOKUP_SET_ID: u16 = 4;
const DYNSET_SET_ID: u16 = 2;
const COMPAT_NAME: u16 = 1;
const COMPAT_REV: u16 = 2;
const COMPAT_TYPE: u16 = 3;

const NFT_CHAIN_BASE: u32 = 1;
const NFT_SET_ANONYMOUS: u32 = 1;
const NFT_SET_INTERVAL: u32 = 4;
const NFT_SET_ELEM_INTERVAL_END: u32 = 1;
const NFT_JUMP: i32 = -3;
const NFT_GOTO: i32 = -4;
/// Maximum length of a name (`NFT_NAME_MAXLEN`, including the terminating NUL).
const NAME_MAX: usize = 256;

/// The ruleset of one network namespace.
#[derive(Debug, Clone)]
pub(crate) struct Nft {
    /// Generation number (`net->nft.base_seq`): starts at 1 in a new namespace and goes up by one
    /// with every committed change.
    pub generation: u32,
    tables: Vec<Table>,
    /// Table handles (`nft_net->table_handle`): a counter in the namespace; deleted ones are
    /// never reused.
    table_handle: u64,
}

impl Default for Nft {
    fn default() -> Nft {
        Nft {
            generation: 1,
            tables: Vec::new(),
            table_handle: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Table {
    family: u8,
    name: Vec<u8>,
    handle: u64,
    flags: u32,
    userdata: Option<Vec<u8>>,
    /// The table's handle counter (`table->hgenerator`): shared by chains, rules and sets.
    hgen: u64,
    chains: Vec<Chain>,
    sets: Vec<Set>,
}

#[derive(Debug, Clone)]
struct Chain {
    name: Vec<u8>,
    handle: u64,
    base: Option<Base>,
    userdata: Option<Vec<u8>>,
    rules: Vec<Rule>,
    /// Id within the batch (`NFTA_CHAIN_ID`): rules in the same batch find the chain by it.
    id: Option<u32>,
}

#[derive(Debug, Clone)]
struct Base {
    hooknum: u32,
    priority: i32,
    ty: Vec<u8>,
    policy: u32,
    /// `NFTA_CHAIN_COUNTERS` was given at creation (dumps include the counters, always 0).
    counters: bool,
}

#[derive(Debug, Clone)]
struct Rule {
    handle: u64,
    /// Contents of `NFTA_RULE_EXPRESSIONS` (a series of `NFTA_LIST_ELEM`).
    exprs: Vec<u8>,
    userdata: Option<Vec<u8>>,
    /// The state of its rate expressions, by their position in the rule; shared with the copies
    /// a batch makes of the rule.
    rates: Rates,
}

/// See [`Rule::rates`].
type Rates = Rc<RefCell<HashMap<usize, Rate>>>;

/// The running state of one rate expression.
#[derive(Debug, Clone, Copy)]
enum Rate {
    /// A token bucket (`nft_limit`, xt's `limit_mt`): nanoseconds of credit, and when it was last
    /// topped up.
    Bucket { tokens: u64, last: Instant },
    /// Bytes counted against a quota (`nft_quota`).
    Used(u64),
}

#[derive(Debug, Clone)]
struct Set {
    name: Vec<u8>,
    handle: u64,
    flags: u32,
    key_len: u32,
    id: Option<u32>,
    /// Other attributes (KEY_TYPE, DATA_TYPE, DATA_LEN, POLICY, DESC, TIMEOUT, USERDATA and so
    /// on), returned as given in dumps.
    other: Vec<(u16, Vec<u8>)>,
    /// Elements (contents of `NFTA_LIST_ELEM`).
    elems: Vec<Vec<u8>>,
}

// ---- Requests ----

/// A failed request: the error code, and the offending attribute (extack's
/// `NLMSGERR_ATTR_OFFS`: nft uses it to mark the offending word in the command).
#[derive(Debug)]
struct Fail {
    errno: i32,
    /// Address within the request of the offending attribute's contents (the slice given by
    /// [`get`]).
    at: Option<usize>,
}

fn fail(e: Errno) -> Fail {
    Fail {
        errno: e.0,
        at: None,
    }
}

/// The offending attribute is `attr` (`NL_SET_BAD_ATTR`).
fn bad(e: Errno, attr: Option<&[u8]>) -> Fail {
    Fail {
        errno: e.0,
        at: attr.map(|a| a.as_ptr() as usize),
    }
}

/// The parts of one request (a whole message).
struct Req<'a> {
    msg: u8,
    flags: u16,
    seq: u32,
    family: u8,
    attrs: Vec<(u16, &'a [u8])>,
}

impl Req<'_> {
    fn dump(&self) -> bool {
        self.flags & NLM_F_DUMP == NLM_F_DUMP
    }
}

/// Where answers go: the socket's `nl_pid`, `NETLINK_EXT_ACK` and `NETLINK_CAP_ACK`, and the
/// requesting process (`PROC_PID` and `PROC_NAME` of `NFT_MSG_NEWGEN`).
pub(crate) struct Peer<'a> {
    pub pid: u32,
    pub ext_ack: bool,
    pub cap_ack: bool,
    pub proc_pid: u32,
    pub comm: &'a [u8],
}

/// Handles one datagram the guest sent on `NETLINK_NETFILTER`, returning the datagrams of the
/// answers.
pub(crate) fn handle(ns: &NetNs, cred: &Cred, data: &[u8], peer: &Peer<'_>) -> Vec<Vec<u8>> {
    let msgs = split(data);
    let Some(first) = msgs.first() else {
        return Vec::new();
    };
    // Needs CAP_NET_ADMIN in the namespace's owner (nfnetlink_rcv: only the first message is
    // acknowledged, the rest are dropped)
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return vec![ack(first, Err(fail(Errno::EPERM)), peer)];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < msgs.len() {
        let m = msgs[i];
        let ty = u16::from_le_bytes([m[4], m[5]]);
        if ty == NFNL_MSG_BATCH_BEGIN {
            let end = msgs[i + 1..]
                .iter()
                .position(|x| u16::from_le_bytes([x[4], x[5]]) == NFNL_MSG_BATCH_END);
            let Some(end) = end.map(|e| i + 1 + e) else {
                // A batch without an end is discarded, and nothing is answered
                return out;
            };
            out.extend(batch(ns, m, &msgs[i + 1..end], peer));
            i = end + 1;
            continue;
        }
        out.extend(single(ns, m, peer));
        i += 1;
    }
    out
}

/// The messages in one datagram.
fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 16 <= data.len() {
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if len < 16 || off + len > data.len() {
            break;
        }
        out.push(&data[off..off + len]);
        off += len.next_multiple_of(4);
    }
    out
}

/// One batch: applied message by message on a copy of the ruleset, which replaces the ruleset
/// only if all succeed.
fn batch(ns: &NetNs, begin: &[u8], msgs: &[&[u8]], peer: &Peer<'_>) -> Vec<Vec<u8>> {
    let b = parse(begin);
    let res = begin
        .get(18..20)
        .map_or(0, |r| u16::from_be_bytes([r[0], r[1]]));
    if res != NFNL_SUBSYS_NFTABLES {
        return vec![ack(begin, Err(fail(Errno::EOPNOTSUPP)), peer)];
    }
    let mut st = ns.nft.borrow().clone();
    if let Some(genid) = get_be32(&b.attrs, NFNL_BATCH_GENID)
        && genid != 0
        && genid != st.generation
    {
        return vec![ack(
            begin,
            Err(Fail {
                errno: ERESTART,
                at: None,
            }),
            peer,
        )];
    }
    let mut replies = Vec::new();
    let mut failed = false;
    let mut changed = false;
    for m in msgs {
        let r = parse(m);
        let ty = u16::from_le_bytes([m[4], m[5]]);
        let res = if ty >> 8 != NFNL_SUBSYS_NFTABLES {
            Err(fail(Errno::EINVAL))
        } else {
            st.apply(&r)
        };
        match res {
            Ok(()) => {
                changed = true;
                if r.flags & NLM_F_ACK != 0 {
                    replies.push(ack(m, Ok(()), peer));
                }
            }
            Err(f) => {
                failed = true;
                replies.push(ack(m, Err(f), peer));
            }
        }
    }
    if !failed {
        if changed {
            st.generation = st.generation.wrapping_add(1).max(1);
        }
        // Ids (NFTA_CHAIN_ID, NFTA_SET_ID) are valid only within one batch
        for t in &mut st.tables {
            t.chains.iter_mut().for_each(|c| c.id = None);
            t.sets.iter_mut().for_each(|s| s.id = None);
        }
        *ns.nft.borrow_mut() = st;
    }
    replies
}

/// One message outside a batch: queries, `NFT_MSG_GETGEN`, xt extension revisions, conntrack.
fn single(ns: &NetNs, m: &[u8], peer: &Peer<'_>) -> Vec<Vec<u8>> {
    let r = parse(m);
    let ty = u16::from_le_bytes([m[4], m[5]]);
    if r.flags & NLM_F_REQUEST == 0 || ty < 0x10 {
        return Vec::new();
    }
    let st = ns.nft.borrow();
    let answer = match ty >> 8 {
        NFNL_SUBSYS_NFTABLES => st.query(&r, peer),
        NFNL_SUBSYS_NFT_COMPAT => compat(&r, peer),
        // The conntrack table is always empty: dumps contain nothing
        NFNL_SUBSYS_CTNETLINK | NFNL_SUBSYS_CTNETLINK_EXP if r.dump() => {
            Ok(vec![done(r.seq, peer.pid)])
        }
        NFNL_SUBSYS_CTNETLINK | NFNL_SUBSYS_CTNETLINK_EXP => Err(fail(Errno::EOPNOTSUPP)),
        _ => Err(fail(Errno::EINVAL)),
    };
    match answer {
        Ok(mut d) => {
            if r.flags & NLM_F_ACK != 0 && !r.dump() {
                d.push(ack(m, Ok(()), peer));
            }
            d
        }
        Err(f) => vec![ack(m, Err(f), peer)],
    }
}

fn parse(m: &[u8]) -> Req<'_> {
    let ty = u16::from_le_bytes([m[4], m[5]]);
    Req {
        msg: ty as u8,
        flags: u16::from_le_bytes([m[6], m[7]]),
        seq: u32::from_le_bytes(m[8..12].try_into().unwrap()),
        family: m.get(16).copied().unwrap_or(0),
        attrs: crate::route::attrs(m.get(20..).unwrap_or_default()),
    }
}

/// The acknowledgement or error for request `req` (a whole message, `netlink_ack`): when the
/// offending attribute is in the request and `NETLINK_EXT_ACK` is set, its offset is attached
/// (counted from the message header, `NLMSGERR_ATTR_OFFS`).
fn ack(req: &[u8], r: Result<(), Fail>, peer: &Peer<'_>) -> Vec<u8> {
    const NLM_F_CAPPED: u16 = 0x100;
    const NLM_F_ACK_TLVS: u16 = 0x200;
    const NLMSGERR_ATTR_OFFS: u16 = 2;
    let (errno, at) = match r {
        Ok(()) => (0, None),
        Err(f) => (f.errno, f.at),
    };
    let base = req.as_ptr() as usize;
    let offs = at
        .filter(|&a| peer.ext_ack && a >= base + 4 && a <= base + req.len())
        .map(|a| (a - 4 - base) as u32);
    let mut p = (-errno).to_le_bytes().to_vec();
    let mut flags = 0;
    if errno == 0 || peer.cap_ack {
        p.extend_from_slice(&req[..16]);
        flags |= NLM_F_CAPPED;
    } else {
        p.extend_from_slice(req);
    }
    if let Some(o) = offs {
        p.resize(p.len().next_multiple_of(4), 0);
        attr(&mut p, NLMSGERR_ATTR_OFFS, &o.to_ne_bytes());
        flags |= NLM_F_ACK_TLVS;
    }
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    message(NLMSG_ERROR, flags, seq, peer.pid, &p)
}

// ---- Changes ----

/// Known expressions (those `nft_expr_type_get` finds, the ones commonly built into 6.1): others
/// are ENOENT.
const EXPRS: [&str; 39] = [
    "bitwise",
    "byteorder",
    "cmp",
    "connlimit",
    "counter",
    "ct",
    "dup",
    "dynset",
    "exthdr",
    "fib",
    "flow_offload",
    "fwd",
    "hash",
    "immediate",
    "last",
    "limit",
    "log",
    "lookup",
    "masq",
    "match",
    "meta",
    "nat",
    "notrack",
    "numgen",
    "objref",
    "osf",
    "payload",
    "queue",
    "quota",
    "range",
    "redir",
    "reject",
    "rt",
    "socket",
    "synproxy",
    "target",
    "tproxy",
    "tunnel",
    "xfrm",
];

impl Nft {
    /// One change in a batch (`nf_tables_newtable` and so on).
    fn apply(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        // nf_tables_deltable: family NFPROTO_UNSPEC, or neither a name nor a handle, flushes
        // instead (`nft flush ruleset`, the first line of most nftables.conf files), for any family
        if matches!(r.msg, DELTABLE | DESTROYTABLE)
            && (r.family == 0
                || get(&r.attrs, TABLE_NAME).is_none() && get(&r.attrs, TABLE_HANDLE).is_none())
        {
            self.flush(r);
            return Ok(());
        }
        if !valid_family(r.family) {
            return Err(fail(Errno::EAFNOSUPPORT));
        }
        if matches!(r.msg, DESTROYTABLE..=DESTROYSETELEM) {
            let del = Req {
                msg: [DELTABLE, DELCHAIN, DELRULE, DELSET, DELSETELEM]
                    [(r.msg - DESTROYTABLE) as usize],
                attrs: r.attrs.clone(),
                ..*r
            };
            return match self.apply(&del) {
                Err(f) if f.errno == Errno::ENOENT.0 => Ok(()),
                x => x,
            };
        }
        match r.msg {
            NEWTABLE => self.new_table(r),
            DELTABLE => {
                let t = self.table_of(r, TABLE_NAME, Some(TABLE_HANDLE))?;
                self.tables.remove(t);
                Ok(())
            }
            NEWCHAIN => self.new_chain(r),
            DELCHAIN => self.del_chain(r),
            NEWRULE => self.new_rule(r),
            DELRULE => self.del_rule(r),
            NEWSET => self.new_set(r),
            DELSET => self.del_set(r),
            NEWSETELEM | DELSETELEM => self.set_elems(r),
            NEWOBJ | NEWFLOWTABLE => Err(fail(Errno::EOPNOTSUPP)),
            // Queries are not in batches (nfnetlink_rcv_batch: callbacks that are not
            // NFNL_CB_BATCH)
            _ => Err(fail(Errno::EINVAL)),
        }
    }

    /// `nft_flush`: deletes every table of the request's family (of all families for
    /// NFPROTO_UNSPEC), only those with its name when it has one; a handle is ignored. Matching
    /// nothing is not an error.
    fn flush(&mut self, r: &Req<'_>) {
        let name = get_str(&r.attrs, TABLE_NAME);
        self.tables.retain(|t| {
            !((r.family == 0 || t.family == r.family) && name.is_none_or(|n| t.name == n))
        });
    }

    fn new_table(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let attr = get(&r.attrs, TABLE_NAME);
        let name = get_str(&r.attrs, TABLE_NAME).ok_or_else(|| fail(Errno::EINVAL))?;
        valid_name(name)?;
        let flags = get_be32(&r.attrs, TABLE_FLAGS).unwrap_or(0);
        // Only NFT_TABLE_F_DORMANT: NFT_TABLE_F_OWNER (the table goes away when the socket
        // closes) is not implemented
        if flags & !1 != 0 {
            return Err(fail(Errno::EOPNOTSUPP));
        }
        if let Some(t) = self.table(r.family, name) {
            if r.flags & NLM_F_EXCL != 0 {
                return Err(bad(Errno::EEXIST, attr));
            }
            if r.flags & NLM_F_REPLACE != 0 {
                return Err(fail(Errno::EOPNOTSUPP));
            }
            self.tables[t].flags = flags;
            return Ok(());
        }
        self.table_handle += 1;
        self.tables.push(Table {
            family: r.family,
            name: name.to_vec(),
            handle: self.table_handle,
            flags,
            userdata: get(&r.attrs, TABLE_USERDATA).map(<[u8]>::to_vec),
            hgen: 0,
            chains: Vec::new(),
            sets: Vec::new(),
        });
        Ok(())
    }

    fn new_chain(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, CHAIN_TABLE, None)?;
        let table = &mut self.tables[t];
        let name = get_str(&r.attrs, CHAIN_NAME);
        let (existing, attr) = match (name, get_be64(&r.attrs, CHAIN_HANDLE)) {
            (_, Some(h)) => {
                let a = get(&r.attrs, CHAIN_HANDLE);
                let c = table.chains.iter().position(|c| c.handle == h);
                (Some(c.ok_or_else(|| bad(Errno::ENOENT, a))?), a)
            }
            (Some(n), None) => (
                table.chains.iter().position(|c| c.name == n),
                get(&r.attrs, CHAIN_NAME),
            ),
            (None, None) => return Err(fail(Errno::EINVAL)),
        };
        let hook = get(&r.attrs, CHAIN_HOOK).map(crate::route::attrs);
        let policy = get_be32(&r.attrs, CHAIN_POLICY);
        let policy_attr = get(&r.attrs, CHAIN_POLICY);
        if let Some(c) = existing {
            if r.flags & NLM_F_EXCL != 0 {
                return Err(bad(Errno::EEXIST, attr));
            }
            if r.flags & NLM_F_REPLACE != 0 {
                return Err(fail(Errno::EOPNOTSUPP));
            }
            let c = &mut table.chains[c];
            if policy.is_some() && c.base.is_none() {
                return Err(bad(Errno::EOPNOTSUPP, policy_attr));
            }
            if policy.is_some_and(|p| p > 1) {
                return Err(fail(Errno::EINVAL));
            }
            // An existing chain: hook, priority and type must match the original (when they
            // differ a 6.8 kernel returns EOPNOTSUPP, and nft then refetches its cache and reports
            // "already exists ... with different declaration" itself)
            if let Some(h) = &hook {
                let Some(b) = &c.base else {
                    return Err(bad(Errno::EEXIST, attr));
                };
                let same = get_be32(h, HOOK_HOOKNUM) == Some(b.hooknum)
                    && get_be32(h, HOOK_PRIORITY) == Some(b.priority as u32)
                    && get_str(&r.attrs, CHAIN_TYPE).is_none_or(|t| t == b.ty);
                if !same {
                    return Err(fail(Errno::EOPNOTSUPP));
                }
            }
            if let (Some(b), Some(p)) = (&mut c.base, policy) {
                b.policy = p;
            }
            if let Some(u) = get(&r.attrs, CHAIN_USERDATA) {
                c.userdata = Some(u.to_vec());
            }
            return Ok(());
        }
        if policy.is_some() && hook.is_none() {
            return Err(bad(Errno::EOPNOTSUPP, policy_attr));
        }
        if policy.is_some_and(|p| p > 1) {
            return Err(fail(Errno::EINVAL));
        }
        let name = name.ok_or_else(|| fail(Errno::EINVAL))?;
        valid_name(name)?;
        let base = match hook {
            Some(h) => {
                let hooknum = get_be32(&h, HOOK_HOOKNUM).ok_or_else(|| fail(Errno::EINVAL))?;
                let priority =
                    get_be32(&h, HOOK_PRIORITY).ok_or_else(|| fail(Errno::EINVAL))? as i32;
                let ty = get_str(&r.attrs, CHAIN_TYPE).unwrap_or(b"filter").to_vec();
                // nat chains can only be on prerouting, input, output and postrouting
                let ok = match &ty[..] {
                    b"filter" => true,
                    b"route" => hooknum == 3,
                    b"nat" => matches!(hooknum, 0 | 1 | 3 | 4),
                    _ => return Err(bad(Errno::ENOENT, get(&r.attrs, CHAIN_TYPE))),
                };
                if !ok || hooknum > 5 {
                    return Err(fail(Errno::EOPNOTSUPP));
                }
                Some(Base {
                    hooknum,
                    priority,
                    ty,
                    policy: policy.unwrap_or(1),
                    counters: get(&r.attrs, CHAIN_COUNTERS).is_some(),
                })
            }
            None => None,
        };
        table.hgen += 1;
        let handle = table.hgen;
        table.chains.push(Chain {
            name: name.to_vec(),
            handle,
            base,
            userdata: get(&r.attrs, CHAIN_USERDATA).map(<[u8]>::to_vec),
            rules: Vec::new(),
            id: get_be32(&r.attrs, CHAIN_ID),
        });
        Ok(())
    }

    fn del_chain(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, CHAIN_TABLE, None)?;
        let table = &mut self.tables[t];
        let (c, attr) = match (
            get_be64(&r.attrs, CHAIN_HANDLE),
            get_str(&r.attrs, CHAIN_NAME),
        ) {
            (Some(h), _) => (
                table.chains.iter().position(|c| c.handle == h),
                get(&r.attrs, CHAIN_HANDLE),
            ),
            (None, Some(n)) => (
                table.chains.iter().position(|c| c.name == n),
                get(&r.attrs, CHAIN_NAME),
            ),
            (None, None) => return Err(fail(Errno::EINVAL)),
        };
        let c = c.ok_or_else(|| bad(Errno::ENOENT, attr))?;
        let name = table.chains[c].name.clone();
        let refs = table.jumps_to(&name);
        // NLM_F_NONREC: not deleted when it has rules or is referenced; otherwise its rules go
        // first, and EBUSY when other chains still jump to it
        if r.flags & NLM_F_NONREC != 0 && refs + table.chains[c].rules.len() > 0 {
            return Err(fail(Errno::EBUSY));
        }
        if refs > 0 {
            return Err(bad(Errno::EBUSY, attr));
        }
        table.chains.remove(c);
        table.drop_unused_anonymous();
        Ok(())
    }

    fn new_rule(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, RULE_TABLE, None)?;
        let table = &mut self.tables[t];
        let c = match (
            get_str(&r.attrs, RULE_CHAIN),
            get_be32(&r.attrs, RULE_CHAIN_ID),
        ) {
            (Some(n), _) => table
                .chains
                .iter()
                .position(|c| c.name == n)
                .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, RULE_CHAIN))),
            (None, Some(id)) => table
                .chains
                .iter()
                .position(|c| c.id == Some(id))
                .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, RULE_CHAIN_ID))),
            (None, None) => Err(fail(Errno::EINVAL)),
        }?;
        let replace = match get_be64(&r.attrs, RULE_HANDLE) {
            Some(h) => {
                let a = get(&r.attrs, RULE_HANDLE);
                let at = table.chains[c]
                    .rules
                    .iter()
                    .position(|x| x.handle == h)
                    .ok_or_else(|| bad(Errno::ENOENT, a))?;
                if r.flags & NLM_F_EXCL != 0 {
                    return Err(bad(Errno::EEXIST, a));
                }
                if r.flags & NLM_F_REPLACE == 0 {
                    return Err(fail(Errno::EOPNOTSUPP));
                }
                Some(at)
            }
            None => None,
        };
        let position = match get_be64(&r.attrs, RULE_POSITION) {
            Some(p) => Some(
                table.chains[c]
                    .rules
                    .iter()
                    .position(|x| x.handle == p)
                    .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, RULE_POSITION)))?,
            ),
            None => None,
        };
        let from = table.chains[c].name.clone();
        let exprs = match get(&r.attrs, RULE_EXPRESSIONS) {
            Some(e) => table.check_exprs(e, &from)?,
            None => Vec::new(),
        };
        let userdata = get(&r.attrs, RULE_USERDATA).map(<[u8]>::to_vec);
        table.hgen += 1;
        let rule = Rule {
            handle: table.hgen,
            exprs,
            userdata,
            rates: Rc::new(RefCell::new(HashMap::new())),
        };
        let rules = &mut table.chains[c].rules;
        match (replace, position) {
            (Some(at), _) => rules[at] = rule,
            (None, Some(p)) if r.flags & NLM_F_APPEND != 0 => rules.insert(p + 1, rule),
            (None, Some(p)) => rules.insert(p, rule),
            (None, None) if r.flags & NLM_F_APPEND != 0 => rules.push(rule),
            (None, None) => rules.insert(0, rule),
        }
        if replace.is_some() {
            table.drop_unused_anonymous();
        }
        Ok(())
    }

    fn del_rule(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, RULE_TABLE, None)?;
        let table = &mut self.tables[t];
        let chain = get_str(&r.attrs, RULE_CHAIN);
        if let Some(n) = chain
            && !table.chains.iter().any(|c| c.name == n)
        {
            return Err(bad(Errno::ENOENT, get(&r.attrs, RULE_CHAIN)));
        }
        let handle = get_be64(&r.attrs, RULE_HANDLE);
        for c in &mut table.chains {
            if chain.is_some_and(|n| c.name != n) {
                continue;
            }
            match handle {
                Some(h) => {
                    let at = c
                        .rules
                        .iter()
                        .position(|x| x.handle == h)
                        .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, RULE_HANDLE)))?;
                    c.rules.remove(at);
                }
                None => c.rules.clear(),
            }
        }
        table.drop_unused_anonymous();
        Ok(())
    }

    fn new_set(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, SET_TABLE, None)?;
        let table = &mut self.tables[t];
        let attr = get(&r.attrs, SET_NAME);
        let name = get_str(&r.attrs, SET_NAME).ok_or_else(|| fail(Errno::EINVAL))?;
        valid_name(name)?;
        let key_len = get_be32(&r.attrs, SET_KEY_LEN).unwrap_or(0);
        if key_len == 0 || key_len > 64 {
            return Err(fail(Errno::EINVAL));
        }
        // A name with %d: take the lowest unused number (anonymous sets are __set%d,
        // nf_tables_set_alloc_name)
        let name = match name.windows(2).position(|w| w == b"%d") {
            Some(at) => {
                let with =
                    |n: usize| [&name[..at], n.to_string().as_bytes(), &name[at + 2..]].concat();
                let n = (0..)
                    .find(|&n| !table.sets.iter().any(|x| x.name == with(n)))
                    .unwrap();
                with(n)
            }
            None => name.to_vec(),
        };
        if table.sets.iter().any(|x| x.name == name) {
            return if r.flags & NLM_F_EXCL != 0 {
                Err(bad(Errno::EEXIST, attr))
            } else {
                Ok(())
            };
        }
        let skip = [
            SET_TABLE,
            SET_NAME,
            SET_FLAGS,
            SET_KEY_LEN,
            SET_ID,
            SET_HANDLE,
        ];
        table.hgen += 1;
        table.sets.push(Set {
            name,
            handle: table.hgen,
            flags: get_be32(&r.attrs, SET_FLAGS).unwrap_or(0),
            key_len,
            id: get_be32(&r.attrs, SET_ID),
            other: r
                .attrs
                .iter()
                .filter(|(t, _)| !skip.contains(t))
                .map(|(t, d)| (*t, d.to_vec()))
                .collect(),
            elems: Vec::new(),
        });
        Ok(())
    }

    fn del_set(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, SET_TABLE, None)?;
        let table = &mut self.tables[t];
        let (s, attr) = match (get_be64(&r.attrs, SET_HANDLE), get_str(&r.attrs, SET_NAME)) {
            (Some(h), _) => (
                table.sets.iter().position(|x| x.handle == h),
                get(&r.attrs, SET_HANDLE),
            ),
            (None, Some(n)) => (
                table.sets.iter().position(|x| x.name == n),
                get(&r.attrs, SET_NAME),
            ),
            (None, None) => return Err(fail(Errno::EINVAL)),
        };
        let s = s.ok_or_else(|| bad(Errno::ENOENT, attr))?;
        if table.uses_set(&table.sets[s].name) {
            return Err(bad(Errno::EBUSY, attr));
        }
        table.sets.remove(s);
        Ok(())
    }

    fn set_elems(&mut self, r: &Req<'_>) -> Result<(), Fail> {
        let t = self.table_of(r, SET_ELEM_LIST_TABLE, None)?;
        let table = &mut self.tables[t];
        let s = match (
            get_str(&r.attrs, SET_ELEM_LIST_SET),
            get_be32(&r.attrs, SET_ELEM_LIST_SET_ID),
        ) {
            (_, Some(id)) if table.sets.iter().any(|x| x.id == Some(id)) => {
                table.sets.iter().position(|x| x.id == Some(id))
            }
            (Some(n), _) => table.sets.iter().position(|x| x.name == n),
            _ => None,
        }
        .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, SET_ELEM_LIST_SET)))?;
        let set = &mut table.sets[s];
        let list = get(&r.attrs, SET_ELEM_LIST_ELEMENTS).map(crate::route::attrs);
        if r.msg == DELSETELEM && list.is_none() {
            set.elems.clear();
            return Ok(());
        }
        for (_, e) in list.unwrap_or_default() {
            let key = elem_key(e).ok_or_else(|| bad(Errno::EINVAL, Some(e)))?;
            let at = set
                .elems
                .iter()
                .position(|x| elem_key(x) == Some(key.clone()) && elem_flags(x) == elem_flags(e));
            match (r.msg, at) {
                (NEWSETELEM, Some(_)) if r.flags & NLM_F_EXCL != 0 => {
                    return Err(bad(Errno::EEXIST, Some(e)));
                }
                (NEWSETELEM, Some(_)) => {}
                (NEWSETELEM, None) => set.elems.push(unflag(e)),
                (_, Some(i)) => {
                    set.elems.remove(i);
                }
                (_, None) => return Err(bad(Errno::ENOENT, Some(e))),
            }
        }
        Ok(())
    }

    /// The table in the request (by name, or by handle when there is no name): ENOENT when not
    /// found, with that attribute as the offending one.
    fn table_of(
        &self,
        r: &Req<'_>,
        name_attr: u16,
        handle_attr: Option<u16>,
    ) -> Result<usize, Fail> {
        if let Some(n) = get_str(&r.attrs, name_attr) {
            return self
                .table(r.family, n)
                .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, name_attr)));
        }
        if let Some(a) = handle_attr
            && let Some(h) = get_be64(&r.attrs, a)
        {
            return self
                .tables
                .iter()
                .position(|t| t.family == r.family && t.handle == h)
                .ok_or_else(|| bad(Errno::ENOENT, get(&r.attrs, a)));
        }
        Err(fail(Errno::EINVAL))
    }

    fn table(&self, family: u8, name: &[u8]) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.family == family && t.name == name)
    }

    /// There are no tables.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// Valid address families (`NFPROTO_*`): inet, ip, arp, netdev, bridge, ip6.
fn valid_family(f: u8) -> bool {
    matches!(f, 1 | 2 | 3 | 5 | 7 | 10)
}

fn valid_name(n: &[u8]) -> Result<(), Fail> {
    if n.is_empty() || n.len() >= NAME_MAX {
        return Err(fail(Errno::EINVAL));
    }
    Ok(())
}

impl Table {
    /// Number of rules jumping to chain `name` (immediate jump and goto).
    fn jumps_to(&self, name: &[u8]) -> usize {
        self.chains
            .iter()
            .flat_map(|c| &c.rules)
            .map(|r| {
                exprs(&r.exprs)
                    .iter()
                    .filter(|(n, d)| n == b"immediate" && verdict_chain(d).as_deref() == Some(name))
                    .count()
            })
            .sum()
    }

    /// Removes the anonymous sets no rule uses any more (they go away with the rules when those
    /// are deleted).
    fn drop_unused_anonymous(&mut self) {
        let unused: Vec<Vec<u8>> = self
            .sets
            .iter()
            .filter(|s| s.flags & NFT_SET_ANONYMOUS != 0 && !self.uses_set(&s.name))
            .map(|s| s.name.clone())
            .collect();
        self.sets.retain(|s| !unused.contains(&s.name));
    }

    /// Whether some rule uses set `name` (lookup, dynset).
    fn uses_set(&self, name: &[u8]) -> bool {
        self.chains.iter().flat_map(|c| &c.rules).any(|r| {
            exprs(&r.exprs).iter().any(|(n, d)| {
                (n == b"lookup" || n == b"dynset")
                    && get_str(&crate::route::attrs(d), LOOKUP_SET) == Some(name)
            })
        })
    }

    /// Checks a rule's expressions (`nf_tables_newrule`): names must be known (ENOENT), jump
    /// targets must exist and must not be base chains, and lookup sets must exist. Returns what to
    /// record: sets referred to by lookup and dynset are replaced by names (anonymous sets referred
    /// to by id), and jumps by id are replaced by names.
    fn check_exprs(&self, e: &[u8], from: &[u8]) -> Result<Vec<u8>, Fail> {
        let mut out = Vec::new();
        for (_, elem) in crate::route::attrs(e) {
            // The offending one is this expression (NL_SET_BAD_ATTR in nf_tables_newrule); a loop
            // belongs to the whole rule
            let here = |f: Fail| {
                if f.errno == Errno::EMLINK.0 {
                    f
                } else {
                    bad(Errno(f.errno), Some(elem))
                }
            };
            let a = crate::route::attrs(elem);
            let name = get_str(&a, EXPR_NAME).ok_or_else(|| bad(Errno::EINVAL, Some(elem)))?;
            if !EXPRS.iter().any(|n| n.as_bytes() == name) {
                return Err(bad(Errno::ENOENT, Some(elem)));
            }
            let data = unflag(get(&a, EXPR_DATA).unwrap_or_default());
            let data = match name {
                b"immediate" => self.check_verdict(&data, from).map_err(here)?,
                b"lookup" => self.check_set_ref(&data, LOOKUP_SET_ID).map_err(here)?,
                b"dynset" => self.check_set_ref(&data, DYNSET_SET_ID).map_err(here)?,
                _ => data,
            };
            let mut x = Vec::new();
            attr(&mut x, EXPR_NAME, &cstr(name));
            attr(&mut x, EXPR_DATA, &data);
            attr(&mut out, LIST_ELEM, &x);
        }
        Ok(out)
    }

    fn check_verdict(&self, data: &[u8], from: &[u8]) -> Result<Vec<u8>, Fail> {
        let a = crate::route::attrs(data);
        let Some(d) = get(&a, IMMEDIATE_DATA) else {
            return Ok(data.to_vec());
        };
        let Some(v) = get(&crate::route::attrs(d), DATA_VERDICT) else {
            return Ok(data.to_vec());
        };
        let va = crate::route::attrs(v);
        let code = get_be32(&va, VERDICT_CODE).ok_or_else(|| fail(Errno::EINVAL))? as i32;
        if code != NFT_JUMP && code != NFT_GOTO {
            return Ok(data.to_vec());
        }
        let chain = match (get_str(&va, VERDICT_CHAIN), get_be32(&va, VERDICT_CHAIN_ID)) {
            (Some(n), _) => self.chains.iter().find(|c| c.name == n),
            (None, Some(id)) => self.chains.iter().find(|c| c.id == Some(id)),
            (None, None) => return Err(fail(Errno::EINVAL)),
        }
        .ok_or_else(|| fail(Errno::ENOENT))?;
        if chain.base.is_some() {
            return Err(fail(Errno::EOPNOTSUPP));
        }
        // Jumping back to itself (directly or through other chains) is a loop: a 6.8 kernel
        // checks the jump depth (nft_chain_validate), and beyond NFT_JUMP_STACK_SIZE it is
        // EMLINK
        if self.reaches(&chain.name, from, 0) {
            return Err(fail(Errno::EMLINK));
        }
        // Record it by name
        let mut nv = Vec::new();
        attr(&mut nv, VERDICT_CODE, &be32(code as u32));
        attr(&mut nv, VERDICT_CHAIN, &cstr(&chain.name));
        let mut nd = Vec::new();
        attr(&mut nd, DATA_VERDICT, &nv);
        let mut out = Vec::new();
        for (t, x) in &a {
            if *t == IMMEDIATE_DATA {
                attr(&mut out, IMMEDIATE_DATA, &nd);
            } else {
                attr(&mut out, *t, x);
            }
        }
        Ok(out)
    }

    /// Whether chain `goal` can be reached from chain `start` through jumps (including itself).
    fn reaches(&self, start: &[u8], goal: &[u8], depth: usize) -> bool {
        if start == goal {
            return true;
        }
        if depth > JUMP_STACK {
            return false;
        }
        let Some(c) = self.chains.iter().find(|c| c.name == start) else {
            return false;
        };
        c.rules.iter().any(|r| {
            exprs(&r.exprs).iter().any(|(n, d)| {
                n == b"immediate"
                    && verdict_chain(d).is_some_and(|t| self.reaches(&t, goal, depth + 1))
            })
        })
    }

    /// The set referred to by lookup or dynset: found by id (an anonymous set just created in the
    /// same batch) or by name; the name is recorded and put first (the kernel orders it that way
    /// in dumps).
    fn check_set_ref(&self, data: &[u8], id_attr: u16) -> Result<Vec<u8>, Fail> {
        let a = crate::route::attrs(data);
        let by_id =
            get_be32(&a, id_attr).and_then(|id| self.sets.iter().find(|s| s.id == Some(id)));
        let set = match by_id {
            Some(s) => s,
            None => {
                let n = get_str(&a, LOOKUP_SET).ok_or_else(|| fail(Errno::EINVAL))?;
                self.sets
                    .iter()
                    .find(|s| s.name == n)
                    .ok_or_else(|| fail(Errno::ENOENT))?
            }
        };
        let mut out = Vec::new();
        attr(&mut out, LOOKUP_SET, &cstr(&set.name));
        for (t, x) in &a {
            if *t != LOOKUP_SET && *t != id_attr {
                attr(&mut out, *t, x);
            }
        }
        Ok(out)
    }
}

/// The chain jumped to in immediate's arguments.
fn verdict_chain(data: &[u8]) -> Option<Vec<u8>> {
    let d = get(&crate::route::attrs(data), IMMEDIATE_DATA)?;
    let v = get(&crate::route::attrs(d), DATA_VERDICT)?;
    get_str(&crate::route::attrs(v), VERDICT_CHAIN).map(<[u8]>::to_vec)
}

// ---- Queries ----

/// xt extensions (`NFNL_MSG_COMPAT_GET` of `nft_compat`): name, whether it is a target, highest
/// revision.
const XT: [(&str, bool, u32); 52] = [
    ("DNAT", true, 2),
    ("SNAT", true, 2),
    ("MASQUERADE", true, 0),
    ("REDIRECT", true, 0),
    ("REJECT", true, 0),
    ("LOG", true, 0),
    ("NFLOG", true, 0),
    ("MARK", true, 2),
    ("CONNMARK", true, 2),
    ("CT", true, 2),
    ("NOTRACK", true, 0),
    ("TCPMSS", true, 0),
    ("CHECKSUM", true, 0),
    ("NFQUEUE", true, 3),
    ("TPROXY", true, 1),
    ("NETMAP", true, 0),
    ("TEE", true, 1),
    ("CLASSIFY", true, 0),
    ("DSCP", true, 0),
    ("TOS", true, 0),
    ("TRACE", true, 0),
    ("SET", true, 3),
    ("TTL", true, 0),
    ("HL", true, 0),
    ("udp", false, 0),
    ("tcp", false, 0),
    ("icmp", false, 0),
    ("icmp6", false, 0),
    ("conntrack", false, 3),
    ("state", false, 0),
    ("comment", false, 0),
    ("addrtype", false, 1),
    ("mark", false, 1),
    ("connmark", false, 1),
    ("multiport", false, 1),
    ("set", false, 4),
    ("physdev", false, 0),
    ("mac", false, 0),
    ("limit", false, 0),
    ("iprange", false, 1),
    ("owner", false, 1),
    ("pkttype", false, 0),
    ("statistic", false, 0),
    ("string", false, 1),
    ("length", false, 0),
    ("hashlimit", false, 3),
    ("recent", false, 1),
    ("socket", false, 3),
    ("policy", false, 0),
    ("devgroup", false, 0),
    ("cgroup", false, 2),
    ("u32", false, 0),
];

impl Nft {
    /// nftables messages outside batches: `NFT_MSG_GET*`.
    fn query(&self, r: &Req<'_>, peer: &Peer<'_>) -> Result<Vec<Vec<u8>>, Fail> {
        let res = (self.generation & 0xffff) as u16;
        let one = |msg: u8, flags: u16, family: u8, attrs: &[u8]| {
            nfmsg(nft_type(msg), flags, r.seq, peer.pid, family, res, attrs)
        };
        // A dump with address family NFPROTO_UNSPEC lists everything
        let fam = |t: &Table| r.family == 0 || t.family == r.family;
        let by_name = |a: u16| get_str(&r.attrs, a);
        match r.msg {
            GETGEN => {
                let mut p = Vec::new();
                attr(&mut p, GEN_ID, &be32(self.generation));
                attr(&mut p, GEN_PROC_PID, &be32(peer.proc_pid));
                attr(&mut p, GEN_PROC_NAME, &cstr(peer.comm));
                Ok(vec![one(NEWGEN, 0, 0, &p)])
            }
            GETTABLE if r.dump() => {
                let m = self
                    .tables
                    .iter()
                    .filter(|t| fam(t))
                    .map(|t| one(NEWTABLE, NLM_F_MULTI, t.family, &t.table_attrs()))
                    .collect();
                Ok(dump_reply(m, r.seq, peer.pid))
            }
            GETTABLE => {
                let t = self.table_of(r, TABLE_NAME, None)?;
                let t = &self.tables[t];
                Ok(vec![one(NEWTABLE, 0, t.family, &t.table_attrs())])
            }
            GETCHAIN if r.dump() => {
                let m = self
                    .tables
                    .iter()
                    .filter(|t| fam(t))
                    .flat_map(|t| {
                        t.chains
                            .iter()
                            .map(|c| one(NEWCHAIN, NLM_F_MULTI, t.family, &t.chain_attrs(c)))
                    })
                    .collect();
                Ok(dump_reply(m, r.seq, peer.pid))
            }
            GETCHAIN => {
                let t = &self.tables[self.table_of(r, CHAIN_TABLE, None)?];
                let n = by_name(CHAIN_NAME).ok_or_else(|| fail(Errno::EINVAL))?;
                let c = t
                    .chains
                    .iter()
                    .find(|c| c.name == n)
                    .ok_or_else(|| fail(Errno::ENOENT))?;
                Ok(vec![one(NEWCHAIN, 0, t.family, &t.chain_attrs(c))])
            }
            GETRULE if r.dump() => {
                let (tn, cn) = (by_name(RULE_TABLE), by_name(RULE_CHAIN));
                let mut m = Vec::new();
                for t in self.tables.iter().filter(|t| fam(t)) {
                    if tn.is_some_and(|n| t.name != n) {
                        continue;
                    }
                    for c in &t.chains {
                        if cn.is_some_and(|n| c.name != n) {
                            continue;
                        }
                        for i in 0..c.rules.len() {
                            let p = t.rule_attrs(c, i, true);
                            m.push(one(NEWRULE, NLM_F_MULTI | NLM_F_APPEND, t.family, &p));
                        }
                    }
                }
                Ok(dump_reply(m, r.seq, peer.pid))
            }
            GETRULE => {
                let t = &self.tables[self.table_of(r, RULE_TABLE, None)?];
                let n = by_name(RULE_CHAIN).ok_or_else(|| fail(Errno::EINVAL))?;
                let c = t
                    .chains
                    .iter()
                    .find(|c| c.name == n)
                    .ok_or_else(|| fail(Errno::ENOENT))?;
                let h = get_be64(&r.attrs, RULE_HANDLE).ok_or_else(|| fail(Errno::EINVAL))?;
                let i = c
                    .rules
                    .iter()
                    .position(|x| x.handle == h)
                    .ok_or_else(|| fail(Errno::ENOENT))?;
                Ok(vec![one(NEWRULE, 0, t.family, &t.rule_attrs(c, i, false))])
            }
            GETSET if r.dump() => {
                let tn = by_name(SET_TABLE);
                let m = self
                    .tables
                    .iter()
                    .filter(|t| fam(t) && tn.is_none_or(|n| t.name == n))
                    .flat_map(|t| {
                        t.sets
                            .iter()
                            .map(|s| one(NEWSET, NLM_F_MULTI, t.family, &t.set_attrs(s)))
                    })
                    .collect();
                Ok(dump_reply(m, r.seq, peer.pid))
            }
            GETSET => {
                let t = &self.tables[self.table_of(r, SET_TABLE, None)?];
                let n = by_name(SET_NAME).ok_or_else(|| fail(Errno::EINVAL))?;
                let s = t
                    .sets
                    .iter()
                    .find(|s| s.name == n)
                    .ok_or_else(|| fail(Errno::ENOENT))?;
                Ok(vec![one(NEWSET, 0, t.family, &t.set_attrs(s))])
            }
            GETSETELEM if r.dump() => {
                let t = &self.tables[self.table_of(r, SET_ELEM_LIST_TABLE, None)?];
                let n = by_name(SET_ELEM_LIST_SET).ok_or_else(|| fail(Errno::EINVAL))?;
                let s = t
                    .sets
                    .iter()
                    .find(|s| s.name == n)
                    .ok_or_else(|| fail(Errno::ENOENT))?;
                // nf_tables_dump_set: one message with the elements when there are any, and
                // always a final one with no elements, together with DONE
                let mut out = Vec::new();
                if !s.elems.is_empty() {
                    out.push(one(
                        NEWSETELEM,
                        NLM_F_MULTI,
                        t.family,
                        &t.elems_attrs(s, &s.elems),
                    ));
                }
                let mut last = one(NEWSETELEM, NLM_F_MULTI, t.family, &t.elems_attrs(s, &[]));
                last.extend(done(r.seq, peer.pid));
                out.push(last);
                Ok(out)
            }
            GETSETELEM => Err(fail(Errno::EOPNOTSUPP)),
            GETOBJ | GETFLOWTABLE if r.dump() => Ok(dump_reply(Vec::new(), r.seq, peer.pid)),
            GETOBJ | GETFLOWTABLE => Err(fail(Errno::ENOENT)),
            // Changes must be in batches
            _ => Err(fail(Errno::EINVAL)),
        }
    }
}

impl Table {
    fn table_attrs(&self) -> Vec<u8> {
        let mut p = Vec::new();
        attr(&mut p, TABLE_NAME, &cstr(&self.name));
        attr(
            &mut p,
            TABLE_USE,
            &be32((self.chains.len() + self.sets.len()) as u32),
        );
        attr(&mut p, TABLE_HANDLE, &be64(self.handle));
        attr(&mut p, TABLE_FLAGS, &be32(self.flags));
        if let Some(u) = &self.userdata {
            attr(&mut p, TABLE_USERDATA, u);
        }
        p
    }

    fn chain_attrs(&self, c: &Chain) -> Vec<u8> {
        let mut p = Vec::new();
        attr(&mut p, CHAIN_TABLE, &cstr(&self.name));
        attr(&mut p, CHAIN_NAME, &cstr(&c.name));
        attr(&mut p, CHAIN_HANDLE, &be64(c.handle));
        if let Some(b) = &c.base {
            let mut h = Vec::new();
            attr(&mut h, HOOK_HOOKNUM, &be32(b.hooknum));
            attr(&mut h, HOOK_PRIORITY, &be32(b.priority as u32));
            attr(&mut p, CHAIN_HOOK, &h);
            attr(&mut p, CHAIN_POLICY, &be32(b.policy));
            attr(&mut p, CHAIN_TYPE, &cstr(&b.ty));
            if b.counters {
                let mut n = Vec::new();
                attr(&mut n, 1, &be64(0));
                attr(&mut n, 2, &be64(0));
                attr(&mut p, CHAIN_COUNTERS, &n);
            }
            attr(&mut p, CHAIN_FLAGS, &be32(NFT_CHAIN_BASE));
        }
        let used = c.rules.len() + self.jumps_to(&c.name);
        attr(&mut p, CHAIN_USE, &be32(used as u32));
        if let Some(u) = &c.userdata {
            attr(&mut p, CHAIN_USERDATA, u);
        }
        p
    }

    /// A rule; `prev` means including the previous rule's handle, as dumps do
    /// (`NFTA_RULE_POSITION`).
    fn rule_attrs(&self, c: &Chain, i: usize, prev: bool) -> Vec<u8> {
        let r = &c.rules[i];
        let mut p = Vec::new();
        attr(&mut p, RULE_TABLE, &cstr(&self.name));
        attr(&mut p, RULE_CHAIN, &cstr(&c.name));
        attr(&mut p, RULE_HANDLE, &be64(r.handle));
        if prev && i > 0 {
            attr(&mut p, RULE_POSITION, &be64(c.rules[i - 1].handle));
        }
        attr(&mut p, RULE_EXPRESSIONS, &r.exprs);
        if let Some(u) = &r.userdata {
            attr(&mut p, RULE_USERDATA, u);
        }
        p
    }

    fn set_attrs(&self, s: &Set) -> Vec<u8> {
        let mut p = Vec::new();
        attr(&mut p, SET_TABLE, &cstr(&self.name));
        attr(&mut p, SET_NAME, &cstr(&s.name));
        attr(&mut p, SET_HANDLE, &be64(s.handle));
        attr(&mut p, SET_FLAGS, &be32(s.flags));
        let other = |t: u16| s.other.iter().find(|(x, _)| *x == t).map(|(_, d)| &d[..]);
        attr(&mut p, SET_KEY_TYPE, other(SET_KEY_TYPE).unwrap_or(&[0; 4]));
        attr(&mut p, SET_KEY_LEN, &be32(s.key_len));
        // The order of nf_tables_fill_set
        for t in [
            SET_DATA_TYPE,
            SET_DATA_LEN,
            SET_OBJ_TYPE,
            SET_TIMEOUT,
            SET_GC_INTERVAL,
            SET_POLICY,
            SET_USERDATA,
        ] {
            if let Some(d) = other(t) {
                attr(&mut p, t, d);
            }
        }
        attr(
            &mut p,
            SET_DESC,
            &other(SET_DESC).map(unflag).unwrap_or_default(),
        );
        for t in [SET_EXPR, SET_EXPRESSIONS] {
            if let Some(d) = other(t) {
                attr(&mut p, t, &unflag(d));
            }
        }
        p
    }

    fn elems_attrs(&self, s: &Set, elems: &[Vec<u8>]) -> Vec<u8> {
        let mut p = Vec::new();
        attr(&mut p, SET_ELEM_LIST_TABLE, &cstr(&self.name));
        attr(&mut p, SET_ELEM_LIST_SET, &cstr(&s.name));
        let mut l = Vec::new();
        for e in elems {
            attr(&mut l, LIST_ELEM, e);
        }
        attr(&mut p, SET_ELEM_LIST_ELEMENTS, &l);
        p
    }
}

/// Packs dumped messages into datagrams (each at most 8 KiB), ending with `NLMSG_DONE`.
fn dump_reply(msgs: Vec<Vec<u8>>, seq: u32, pid: u32) -> Vec<Vec<u8>> {
    const CHUNK: usize = 8192;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut cur = Vec::new();
    for m in msgs {
        if !cur.is_empty() && cur.len() + m.len() > CHUNK {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(m);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.push(done(seq, pid));
    out
}

/// `NFNL_MSG_COMPAT_GET`: whether an xt extension exists, and its highest revision.
fn compat(r: &Req<'_>, peer: &Peer<'_>) -> Result<Vec<Vec<u8>>, Fail> {
    let (Some(name), Some(rev), Some(ty)) = (
        get_str(&r.attrs, COMPAT_NAME),
        get_be32(&r.attrs, COMPAT_REV),
        get_be32(&r.attrs, COMPAT_TYPE),
    ) else {
        return Err(fail(Errno::EINVAL));
    };
    let target = ty == 1;
    let (_, _, max) = XT
        .iter()
        .find(|(n, t, _)| n.as_bytes() == name && *t == target)
        .ok_or_else(|| fail(Errno::ENOENT))?;
    let mut p = Vec::new();
    attr(&mut p, COMPAT_NAME, &cstr(name));
    attr(&mut p, COMPAT_REV, &be32(rev.min(*max)));
    attr(&mut p, COMPAT_TYPE, &be32(ty));
    let t = NFNL_SUBSYS_NFT_COMPAT << 8;
    Ok(vec![nfmsg(
        t,
        NLM_F_MULTI,
        r.seq,
        peer.pid,
        r.family,
        0,
        &p,
    )])
}

// ---- Execution ----

const NF_DROP: i32 = 0;
const NF_ACCEPT: i32 = 1;
const NFT_CONTINUE: i32 = -1;
const NFT_BREAK: i32 = -2;
const NFT_RETURN: i32 = -5;
/// Jump depth (`NFT_JUMP_STACK_SIZE`).
const JUMP_STACK: usize = 16;
/// The PREROUTING and OUTPUT hooks (`NF_INET_PRE_ROUTING`, `NF_INET_LOCAL_OUT`).
pub(crate) const HOOK_PRE_ROUTING: u32 = 0;
pub(crate) const HOOK_LOCAL_OUT: u32 = 3;
const RTN_UNICAST: u32 = 1;
const RTN_LOCAL: u32 = 2;
const RTN_BROADCAST: u32 = 3;
const RTN_MULTICAST: u32 = 5;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

/// A packet being sent (as the socket layer sees it at connect and sendto, with guest addresses).
pub(crate) struct Packet<'a> {
    /// Addresses: 4 bytes for IPv4, 16 bytes for IPv6.
    pub src: &'a [u8],
    pub dst: &'a [u8],
    pub proto: u8,
    pub sport: u16,
    pub dport: u16,
    pub uid: u32,
    pub gid: u32,
    /// The type of an address in this namespace (`RTN_LOCAL` and so on; used by fib's addrtype and
    /// xt's addrtype).
    pub addr_type: &'a dyn Fn(&[u8]) -> u32,
    /// Index and name of the output interface (on OUTPUT and FORWARD; not yet known on
    /// PREROUTING and INPUT, see [`Eval::meta`]).
    pub oif: &'a dyn Fn() -> (u32, Vec<u8>),
    /// Hook: [`HOOK_PRE_ROUTING`], INPUT (1), FORWARD (2) or [`HOOK_LOCAL_OUT`] (POSTROUTING is
    /// never evaluated).
    pub hook: u32,
    /// Input interface (on PREROUTING, INPUT and FORWARD; none on OUTPUT).
    pub iif: Option<Iif>,
    /// Connection tracking state (`NF_CT_STATE_BIT`): the first packet of a new connection is
    /// [`CT_NEW`], and UDP sent back to a peer already received from is [`CT_ESTABLISHED`].
    pub ct_state: u32,
    /// The packet's initial mark (`skb->mark`): the socket's `SO_MARK` for locally sent packets,
    /// cleared when entering another namespace through a veth (`skb_scrub_packet`).
    pub mark: u32,
}

pub(crate) const CT_ESTABLISHED: u32 = 2;
pub(crate) const CT_NEW: u32 = 8;

/// The interface a packet came in on: index, name and its first address (redirect rewrites to
/// this address, `nf_nat_redirect_ipv4`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Iif {
    pub index: u32,
    pub name: Vec<u8>,
    pub addr: Vec<u8>,
}

/// The result of nat chains and filter chains.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Accept,
    Drop,
    /// reject: the error code the initiator gets (translated from ICMP unreachable or TCP RST).
    Reject(i32),
    /// Rewrite the destination to this address and port (`None` keeps the port).
    Dnat(Vec<u8>, Option<u16>),
}

/// The generic part of address types ([`Packet::addr_type`]): multicast and broadcast; the rest is
/// left to `local`.
pub(crate) fn addr_type(a: &[u8], local: impl Fn(&[u8]) -> bool) -> u32 {
    match a.len() {
        4 if a[0] & 0xf0 == 0xe0 => RTN_MULTICAST,
        4 if a == [255; 4] || a == [0; 4] => RTN_BROADCAST,
        16 if a[0] == 0xff => RTN_MULTICAST,
        _ if local(a) => RTN_LOCAL,
        _ => RTN_UNICAST,
    }
}

impl Nft {
    /// Whether hook `hook` has a nat-type base chain (if not, no packet needs to be synthesized).
    pub(crate) fn has_nat(&self, hook: u32) -> bool {
        self.has(hook, true)
    }

    /// Whether hook `hook` has a filter- or route-type base chain.
    pub(crate) fn has_filter(&self, hook: u32) -> bool {
        self.has(hook, false)
    }

    fn has(&self, hook: u32, nat: bool) -> bool {
        self.tables.iter().any(|t| {
            t.flags & 1 == 0
                && t.chains.iter().any(|c| {
                    c.base
                        .as_ref()
                        .is_some_and(|b| b.hooknum == hook && (b.ty == b"nat") == nat)
                })
        })
    }

    /// Runs the nat chains on hook `p.hook` (`nf_nat_inet_fn`): one by one by priority, stopping
    /// at the first that DNATs or drops. Address families: IPv4 packets look at ip and inet
    /// tables, IPv6 ones at ip6 and inet tables.
    pub(crate) fn nat(&self, p: &Packet<'_>) -> Outcome {
        let fam = if p.dst.len() == 4 { 2 } else { 10 };
        let mut chains: Vec<(&Table, &Chain, i32)> = Vec::new();
        for t in &self.tables {
            if (t.family != fam && t.family != 1) || t.flags & 1 != 0 {
                continue;
            }
            for c in &t.chains {
                if let Some(b) = &c.base
                    && b.hooknum == p.hook
                    && b.ty == b"nat"
                {
                    chains.push((t, c, b.priority));
                }
            }
        }
        if chains.is_empty() {
            return Outcome::Accept;
        }
        chains.sort_by_key(|(_, _, prio)| *prio);
        let (nh, th) = headers(p);
        let mut mark = p.mark;
        for (t, c, _) in chains {
            let mut e = Eval {
                p,
                nh: nh.clone(),
                th: th.clone(),
                regs: [0; 80],
                verdict: NFT_CONTINUE,
                chain: None,
                mark,
                dnat: None,
                reject: None,
                rates: None,
                at: 0,
            };
            let v = run_chain(&mut e, t, c);
            mark = e.mark;
            if v == NF_DROP {
                return e.reject.map_or(Outcome::Drop, Outcome::Reject);
            }
            if let Some((a, port)) = e.dnat {
                return Outcome::Dnat(a, port);
            }
        }
        Outcome::Accept
    }

    /// Runs the filter- and route-type chains on hook `p.hook`: one by one by priority, stopping
    /// at the first drop (reject), and running the next one after an accept (each base chain gives
    /// its own verdict).
    pub(crate) fn filter(&self, p: &Packet<'_>) -> Outcome {
        let fam = if p.dst.len() == 4 { 2 } else { 10 };
        let mut chains: Vec<(&Table, &Chain, i32)> = Vec::new();
        for t in &self.tables {
            if (t.family != fam && t.family != 1) || t.flags & 1 != 0 {
                continue;
            }
            for c in &t.chains {
                if let Some(b) = &c.base
                    && b.hooknum == p.hook
                    && b.ty != b"nat"
                {
                    chains.push((t, c, b.priority));
                }
            }
        }
        chains.sort_by_key(|(_, _, prio)| *prio);
        let (nh, th) = headers(p);
        let mut mark = p.mark;
        for (t, c, _) in chains {
            let mut e = Eval {
                p,
                nh: nh.clone(),
                th: th.clone(),
                regs: [0; 80],
                verdict: NFT_CONTINUE,
                chain: None,
                mark,
                dnat: None,
                reject: None,
                rates: None,
                at: 0,
            };
            let v = run_chain(&mut e, t, c);
            mark = e.mark;
            if v == NF_DROP {
                return e.reject.map_or(Outcome::Drop, Outcome::Reject);
            }
        }
        Outcome::Accept
    }

    /// Whether IPv4 (with `v6`, IPv6) packets in this namespace go through connection tracking:
    /// Linux attaches the conntrack hooks only when the ruleset has expressions that use it
    /// (`nf_ct_netns_get`, called when an expression is initialized, dormant tables included); ip
    /// and ip6 tables each cover their own family, and inet tables cover both. An empty nat chain
    /// does not count (checked against native). `SO_ORIGINAL_DST` answers by it for connections
    /// that were not NATed ([`crate::origdst`]).
    pub(crate) fn tracks(&self, v6: bool) -> bool {
        /// Expressions, xt targets and matches that call `nf_ct_netns_get` when initialized.
        const EXPRS: [&[u8]; 7] = [
            b"ct",
            b"nat",
            b"masq",
            b"redir",
            b"connlimit",
            b"flow_offload",
            b"synproxy",
        ];
        const XT: [&[u8]; 10] = [
            b"DNAT",
            b"SNAT",
            b"MASQUERADE",
            b"REDIRECT",
            b"NETMAP",
            b"CONNMARK",
            b"CT",
            b"conntrack",
            b"state",
            b"connmark",
        ];
        let fam = if v6 { 10 } else { 2 };
        self.tables
            .iter()
            .filter(|t| t.family == fam || t.family == 1)
            .flat_map(|t| &t.chains)
            .flat_map(|c| &c.rules)
            .any(|r| {
                exprs(&r.exprs).iter().any(|(name, data)| match &name[..] {
                    b"target" | b"match" => {
                        let a = crate::route::attrs(data);
                        get_str(&a, COMPAT_NAME).is_some_and(|n| XT.contains(&n))
                    }
                    n => EXPRS.contains(&n),
                })
            })
    }
}

fn headers(p: &Packet<'_>) -> (Vec<u8>, Vec<u8>) {
    let th: Vec<u8> = match p.proto {
        IPPROTO_TCP => {
            let mut t = vec![0; 20];
            t[..2].copy_from_slice(&p.sport.to_be_bytes());
            t[2..4].copy_from_slice(&p.dport.to_be_bytes());
            t[12] = 0x50;
            t[13] = 0x02; // SYN
            t[14..16].copy_from_slice(&64240u16.to_be_bytes());
            t
        }
        IPPROTO_UDP => {
            let mut t = vec![0; 8];
            t[..2].copy_from_slice(&p.sport.to_be_bytes());
            t[2..4].copy_from_slice(&p.dport.to_be_bytes());
            t[4..6].copy_from_slice(&8u16.to_be_bytes());
            t
        }
        _ => Vec::new(),
    };
    let nh = if p.dst.len() == 4 {
        let mut h = vec![0; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&((20 + th.len()) as u16).to_be_bytes());
        h[6] = 0x40; // DF
        h[8] = 64;
        h[9] = p.proto;
        h[12..16].copy_from_slice(p.src);
        h[16..20].copy_from_slice(p.dst);
        h
    } else {
        let mut h = vec![0; 40];
        h[0] = 0x60;
        h[4..6].copy_from_slice(&(th.len() as u16).to_be_bytes());
        h[6] = p.proto;
        h[7] = 64;
        h[8..24].copy_from_slice(p.src);
        h[24..40].copy_from_slice(p.dst);
        h
    };
    (nh, th)
}

/// Runs from base chain `base` (`nft_do_chain`), returning the final verdict.
fn run_chain(e: &mut Eval<'_>, t: &Table, base: &Chain) -> i32 {
    let mut stack: Vec<(&Chain, usize)> = Vec::new();
    let (mut chain, mut next) = (base, 0usize);
    // Guards against accidental loops (Table::check_verdict already rejects jumps that loop when
    // a rule is added, as Linux does)
    let mut budget = 100_000;
    loop {
        if next >= chain.rules.len() {
            match stack.pop() {
                Some((c, i)) => {
                    chain = c;
                    next = i;
                    continue;
                }
                None => return base.base.as_ref().map_or(NF_ACCEPT, |b| b.policy as i32),
            }
        }
        budget -= 1;
        if budget == 0 {
            return NF_ACCEPT;
        }
        let rule = &chain.rules[next];
        next += 1;
        e.verdict = NFT_CONTINUE;
        e.chain = None;
        e.rates = Some(rule.rates.clone());
        for (at, (name, data)) in exprs(&rule.exprs).into_iter().enumerate() {
            e.at = at;
            e.expr(t, &name, &data);
            if e.verdict != NFT_CONTINUE {
                break;
            }
        }
        match e.verdict {
            NFT_CONTINUE | NFT_BREAK => {}
            NFT_JUMP | NFT_GOTO => {
                let Some(target) = e
                    .chain
                    .as_ref()
                    .and_then(|n| t.chains.iter().find(|c| &c.name == n))
                else {
                    return NF_DROP;
                };
                if e.verdict == NFT_JUMP {
                    if stack.len() >= JUMP_STACK {
                        return NF_DROP;
                    }
                    stack.push((chain, next));
                }
                chain = target;
                next = 0;
            }
            NFT_RETURN => match stack.pop() {
                Some((c, i)) => {
                    chain = c;
                    next = i;
                }
                None => return base.base.as_ref().map_or(NF_ACCEPT, |b| b.policy as i32),
            },
            v => return v,
        }
    }
}

// ---- Expressions ----

/// The state of a rule while it runs (`struct nft_regs` and the packet).
struct Eval<'a> {
    p: &'a Packet<'a>,
    /// Network- and transport-layer headers (synthesized from the [`Packet`]: TCP is a SYN).
    nh: Vec<u8>,
    th: Vec<u8>,
    /// Registers (20 of 32 bits; the first 4 are for the verdict).
    regs: [u8; 80],
    verdict: i32,
    chain: Option<Vec<u8>>,
    mark: u32,
    dnat: Option<(Vec<u8>, Option<u16>)>,
    /// The reject error code (the verdict is NF_DROP at the same time).
    reject: Option<i32>,
    /// The running rule's rate state and the position of the running expression in it.
    rates: Option<Rates>,
    at: usize,
}

impl Eval<'_> {
    /// One expression; unknown ones and those that cannot be evaluated here count as the rule not
    /// matching (NFT_BREAK).
    fn expr(&mut self, table: &Table, name: &[u8], data: &[u8]) {
        let a = crate::route::attrs(data);
        let u = |t: u16| get_be32(&a, t).unwrap_or(0);
        let ok = match name {
            b"counter" | b"log" | b"notrack" | b"dynset" | b"objref" | b"last" | b"masq"
            | b"connlimit" | b"dup" => true,
            b"limit" => self.limit(&a),
            b"quota" => self.quota(&a),
            b"reject" => {
                self.reject = Some(reject_errno(
                    u(1),
                    get(&a, 2).and_then(|c| c.first().copied()).unwrap_or(0),
                    self.p.dst.len() == 16,
                ));
                self.verdict = NF_DROP;
                true
            }
            b"queue" => {
                self.verdict = NF_DROP;
                true
            }
            b"immediate" => self.immediate(&a),
            b"payload" => self.payload(&a),
            b"cmp" => self.cmp(&a),
            b"bitwise" => self.bitwise(&a),
            b"range" => self.range(&a),
            b"meta" => self.meta(u(1), u(2), get(&a, 3).map(|_| u(3))),
            b"ct" => match (get(&a, 4), u(2)) {
                (Some(_), _) => true,
                // State: NEW, or ESTABLISHED for UDP sent back to a peer it received from
                // (Packet::ct_state); direction ORIGINAL; status and mark 0
                (None, 0) => self.store_u32(u(1), self.p.ct_state),
                (None, 1) => self.store(u(1), &[0]),
                (None, 2 | 3) => self.store_u32(u(1), 0),
                _ => false,
            },
            b"fib" => self.fib(u(1), u(2), u(3)),
            b"lookup" => self.lookup(table, &a),
            b"nat" => self.nat(&a),
            b"redir" => self.redir(get(&a, 1).map(|_| u(1))),
            b"target" => self.target(&a),
            b"match" => self.xt_match(&a),
            _ => false,
        };
        if !ok && self.verdict == NFT_CONTINUE {
            self.verdict = NFT_BREAK;
        }
    }

    fn immediate(&mut self, a: &[(u16, &[u8])]) -> bool {
        let Some(d) = get(a, IMMEDIATE_DATA) else {
            return false;
        };
        let d = crate::route::attrs(d);
        match get_be32(a, IMMEDIATE_DREG) {
            Some(0) => {
                let Some(v) = get(&d, DATA_VERDICT) else {
                    return false;
                };
                let v = crate::route::attrs(v);
                self.verdict = get_be32(&v, VERDICT_CODE).unwrap_or(0) as i32;
                self.chain = get_str(&v, VERDICT_CHAIN).map(<[u8]>::to_vec);
                true
            }
            Some(r) => get(&d, DATA_VALUE).is_some_and(|v| self.store(r, v)),
            None => false,
        }
    }

    fn payload(&mut self, a: &[(u16, &[u8])]) -> bool {
        // Packet mangling (NFTA_PAYLOAD_SREG) is not done
        if get(a, 5).is_some() {
            return false;
        }
        let (Some(dreg), Some(base), Some(off), Some(len)) = (
            get_be32(a, 1),
            get_be32(a, 2),
            get_be32(a, 3),
            get_be32(a, 4),
        ) else {
            return false;
        };
        let hdr = match base {
            1 => &self.nh,
            2 => &self.th,
            _ => return false,
        };
        let (off, len) = (off as usize, len as usize);
        let Some(bytes) = hdr.get(off..off + len).map(<[u8]>::to_vec) else {
            return false;
        };
        self.store(dreg, &bytes)
    }

    fn cmp(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(sreg), Some(op), Some(d)) = (get_be32(a, 1), get_be32(a, 2), get(a, 3)) else {
            return false;
        };
        let Some(v) = data_value(d) else {
            return false;
        };
        let Some(x) = self.load(sreg, v.len()) else {
            return false;
        };
        let o = x.cmp(&v[..]);
        use std::cmp::Ordering::*;
        match op {
            0 => o == Equal,
            1 => o != Equal,
            2 => o == Less,
            3 => o != Greater,
            4 => o == Greater,
            5 => o != Less,
            _ => false,
        }
    }

    fn bitwise(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(sreg), Some(dreg), Some(len)) = (get_be32(a, 1), get_be32(a, 2), get_be32(a, 3))
        else {
            return false;
        };
        let len = len as usize;
        let Some(src) = self.load(sreg, len).map(<[u8]>::to_vec) else {
            return false;
        };
        let out = match get_be32(a, 6).unwrap_or(0) {
            // NFT_BITWISE_BOOL: (src & mask) ^ xor
            0 => {
                let (Some(m), Some(x)) = (
                    get(a, 4).and_then(data_value),
                    get(a, 5).and_then(data_value),
                ) else {
                    return false;
                };
                if m.len() < len || x.len() < len {
                    return false;
                }
                (0..len)
                    .map(|i| (src[i] & m[i]) ^ x[i])
                    .collect::<Vec<u8>>()
            }
            // Left and right shifts: on 32-bit words in host byte order
            op @ (1 | 2) => {
                let Some(s) = get(a, 7).and_then(data_value).and_then(|d| {
                    d.get(..4)
                        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
                }) else {
                    return false;
                };
                if s >= 32 || !len.is_multiple_of(4) {
                    return false;
                }
                let w: Vec<u32> = src
                    .chunks(4)
                    .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                    .collect();
                let mut o = vec![0u32; w.len()];
                let mut carry = 0u32;
                if op == 1 {
                    for i in (0..w.len()).rev() {
                        o[i] = (w[i] << s) | carry;
                        carry = if s == 0 { 0 } else { w[i] >> (32 - s) };
                    }
                } else {
                    for i in 0..w.len() {
                        o[i] = carry | (w[i] >> s);
                        carry = if s == 0 { 0 } else { w[i] << (32 - s) };
                    }
                }
                o.iter().flat_map(|x| x.to_ne_bytes()).collect()
            }
            _ => return false,
        };
        self.store(dreg, &out)
    }

    fn range(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(sreg), Some(from), Some(to)) = (
            get_be32(a, 1),
            get(a, 3).and_then(data_value),
            get(a, 4).and_then(data_value),
        ) else {
            return false;
        };
        let Some(x) = self.load(sreg, from.len()) else {
            return false;
        };
        let inside = x >= &from[..] && x <= &to[..];
        inside == (get_be32(a, 2).unwrap_or(0) == 0)
    }

    fn meta(&mut self, dreg: u32, key: u32, sreg: Option<u32>) -> bool {
        if let Some(s) = sreg {
            // meta set: only mark affects later matching
            if key == 3
                && let Some(v) = self.load(s, 4)
            {
                self.mark = u32::from_ne_bytes(v.try_into().unwrap());
            }
            return true;
        }
        let v6 = self.p.dst.len() == 16;
        match key {
            0 => self.store_u32(dreg, (self.nh.len() + self.th.len()) as u32),
            1 => self.store(dreg, if v6 { &[0x86, 0xdd] } else { &[0x08, 0x00] }),
            3 => self.store_u32(dreg, self.mark),
            // No input interface on OUTPUT, and no output interface on PREROUTING and INPUT:
            // index 0 and an empty name (nft_meta_store_ifname)
            4 => self.store_u32(dreg, self.p.iif.as_ref().map_or(0, |i| i.index)),
            6 => {
                let mut n = self
                    .p
                    .iif
                    .as_ref()
                    .map(|i| i.name.clone())
                    .unwrap_or_default();
                n.resize(16, 0);
                self.store(dreg, &n)
            }
            // No output interface yet on PREROUTING and INPUT
            5 | 7 => {
                let (index, mut n) = if matches!(self.p.hook, HOOK_PRE_ROUTING | 1) {
                    (0, Vec::new())
                } else {
                    (self.p.oif)()
                };
                if key == 5 {
                    return self.store_u32(dreg, index);
                }
                n.resize(16, 0);
                self.store(dreg, &n)
            }
            // Packets on PREROUTING have no socket (unavailable means no match)
            10 if self.p.hook == HOOK_LOCAL_OUT => self.store_u32(dreg, self.p.uid),
            11 if self.p.hook == HOOK_LOCAL_OUT => self.store_u32(dreg, self.p.gid),
            15 => self.store(dreg, &[if v6 { 10 } else { 2 }]),
            16 => self.store(dreg, &[self.p.proto]),
            // PACKET_HOST
            19 => self.store(dreg, &[0]),
            _ => false,
        }
    }

    fn fib(&mut self, dreg: u32, result: u32, flags: u32) -> bool {
        // Only the address type: NFT_FIB_RESULT_ADDRTYPE, by source address (NFTA_FIB_F_SADDR)
        // or destination address
        if result != 3 {
            return false;
        }
        let a = if flags & 1 != 0 {
            self.p.src
        } else {
            self.p.dst
        };
        let t = (self.p.addr_type)(a);
        self.store_u32(dreg, t)
    }

    fn lookup(&mut self, table: &Table, a: &[(u16, &[u8])]) -> bool {
        let (Some(name), Some(sreg)) = (get_str(a, LOOKUP_SET), get_be32(a, 2)) else {
            return false;
        };
        let Some(set) = table.sets.iter().find(|s| s.name == name) else {
            return false;
        };
        let Some(key) = self.load(sreg, set.key_len as usize).map(<[u8]>::to_vec) else {
            return false;
        };
        let found = set_find(set, &key);
        let inv = get_be32(a, 5).unwrap_or(0) & 1 != 0;
        if found.is_some() == inv {
            return false;
        }
        // A map: write the value into the destination register (0 is the verdict)
        match (get_be32(a, 3), found) {
            (Some(dreg), Some(e)) => {
                let Some(d) = get(&crate::route::attrs(&e), 2).map(<[u8]>::to_vec) else {
                    return false;
                };
                let d = crate::route::attrs(&d);
                if dreg == 0 {
                    let Some(v) = get(&d, DATA_VERDICT) else {
                        return false;
                    };
                    let v = crate::route::attrs(v);
                    self.verdict = get_be32(&v, VERDICT_CODE).unwrap_or(0) as i32;
                    self.chain = get_str(&v, VERDICT_CHAIN).map(<[u8]>::to_vec);
                    true
                } else {
                    get(&d, DATA_VALUE).is_some_and(|v| self.store(dreg, v))
                }
            }
            _ => true,
        }
    }

    /// nftables' own nat (`dnat to`): the address and port are in registers.
    fn nat(&mut self, a: &[(u16, &[u8])]) -> bool {
        // NFT_NAT_DNAT; there is no SNAT on OUTPUT
        if get_be32(a, 1) != Some(1) {
            return true;
        }
        let alen = if get_be32(a, 2) == Some(10) { 16 } else { 4 };
        if alen != self.p.dst.len() {
            return false;
        }
        let addr = match get_be32(a, 3) {
            Some(r) => match self.load(r, alen) {
                Some(x) => Some(x.to_vec()),
                None => return false,
            },
            None => None,
        };
        let port = match get_be32(a, 5) {
            Some(r) => match self.load(r, 2) {
                Some(x) => Some(u16::from_be_bytes([x[0], x[1]])),
                None => return false,
            },
            None => None,
        };
        self.set_dnat(addr, port)
    }

    /// `redirect`: rewrites to the loopback address on OUTPUT (LOCAL_OUT in
    /// nf_nat_redirect_ipv4).
    fn redir(&mut self, preg: Option<u32>) -> bool {
        let port = match preg {
            Some(r) => match self.load(r, 2) {
                Some(x) => Some(u16::from_be_bytes([x[0], x[1]])),
                None => return false,
            },
            None => None,
        };
        let to = self.redirect_addr();
        self.set_dnat(Some(to), port)
    }

    /// xt targets (iptables-nft's `-j DNAT`, `-j REDIRECT`). Other targets do not change the
    /// destination address and count as continue.
    fn target(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(name), Some(info)) = (get_str(a, 1), get(a, 3)) else {
            return false;
        };
        let rev = get_be32(a, 2).unwrap_or(0);
        let v4 = self.p.dst.len() == 4;
        let ne32 = |b: &[u8], at: usize| {
            b.get(at..at + 4)
                .map(|x| u32::from_ne_bytes(x.try_into().unwrap()))
        };
        let be16 =
            |b: &[u8], at: usize| b.get(at..at + 2).map(|x| u16::from_be_bytes([x[0], x[1]]));
        // (flags, address, port): rev 0 is nf_nat_ipv4_multi_range_compat, revs 1 and 2 are
        // nf_nat_range and nf_nat_range2
        let range = |rev: u32| -> Option<(u32, Vec<u8>, u16)> {
            if rev == 0 && v4 {
                Some((ne32(info, 4)?, info.get(8..12)?.to_vec(), be16(info, 16)?))
            } else if rev == 0 {
                None
            } else {
                let alen = if v4 { 4 } else { 16 };
                Some((
                    ne32(info, 0)?,
                    info.get(4..4 + alen)?.to_vec(),
                    be16(info, 36)?,
                ))
            }
        };
        match name {
            b"DNAT" => {
                let Some((flags, addr, port)) = range(rev) else {
                    return false;
                };
                // NF_NAT_RANGE_MAP_IPS, NF_NAT_RANGE_PROTO_SPECIFIED
                let addr = (flags & 1 != 0).then_some(addr);
                let port = (flags & 2 != 0).then_some(port);
                self.set_dnat(addr, port)
            }
            b"REDIRECT" => {
                // IPv4 uses the rev 0 compat structure, IPv6 nf_nat_range
                let Some((flags, _, port)) = range(if v4 { 0 } else { 1 }) else {
                    return false;
                };
                let port = (flags & 2 != 0).then_some(port);
                let to = self.redirect_addr();
                self.set_dnat(Some(to), port)
            }
            b"MARK" if rev >= 1 => {
                // xt_mark_tginfo2: mark = (mark & ~mask) ^ value
                let (Some(v), Some(m)) = (ne32(info, 0), ne32(info, 4)) else {
                    return false;
                };
                self.mark = (self.mark & !m) ^ v;
                true
            }
            b"ACCEPT" => {
                self.verdict = NF_ACCEPT;
                true
            }
            b"DROP" => {
                self.verdict = NF_DROP;
                true
            }
            b"REJECT" => {
                // ipt_reject_info: with (ipt_reject_with); IPv6's ip6t_reject_with differs, so it
                // is treated as port unreachable
                let with = ne32(info, 0).unwrap_or(3);
                let e = if v4 {
                    match with {
                        0 | 5 => Errno::ENETUNREACH.0,
                        1 | 6 | 8 => Errno::EHOSTUNREACH.0,
                        2 => Errno::ENOPROTOOPT.0,
                        _ => Errno::ECONNREFUSED.0,
                    }
                } else {
                    Errno::ECONNREFUSED.0
                };
                self.reject = Some(e);
                self.verdict = NF_DROP;
                true
            }
            _ => true,
        }
    }

    /// xt matches: comment, mark, owner, conntrack, state, addrtype, tcp, udp; others count as not
    /// matching.
    /// `limit` (`nft_limit_init`, `nft_limit_eval`): `rate` packets (or bytes) per `unit` seconds,
    /// with room for `burst` more (5 packets when 0); within the rate it matches and beyond it
    /// not, or the reverse with `NFT_LIMIT_F_INV` (`limit rate over`).
    fn limit(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(rate), Some(unit)) = (get_be64(a, 1), get_be64(a, 2)) else {
            return false;
        };
        let burst = u64::from(get_be32(a, 3).unwrap_or(0));
        let bytes = get_be32(a, 4) == Some(1);
        let invert = get_be32(a, 5).unwrap_or(0) & 1 != 0;
        let Some(nsecs) = unit.checked_mul(1_000_000_000).filter(|_| rate != 0) else {
            return false;
        };
        let (cost, max) = if bytes {
            let len = (self.nh.len() + self.th.len()) as u64;
            (
                nsecs.saturating_mul(len) / rate,
                nsecs.saturating_mul(rate.saturating_add(burst)) / rate,
            )
        } else {
            let burst = if burst == 0 { 5 } else { burst };
            (nsecs / rate, (nsecs / rate).saturating_mul(burst))
        };
        self.take_tokens(cost, max) != invert
    }

    /// Takes `cost` nanoseconds of credit from the running expression's bucket, which fills at one
    /// per nanosecond up to `max` and starts full: whether there was enough.
    fn take_tokens(&mut self, cost: u64, max: u64) -> bool {
        let Some(rates) = &self.rates else {
            return true;
        };
        let now = Instant::now();
        let mut rates = rates.borrow_mut();
        let rate = rates.entry(self.at).or_insert(Rate::Bucket {
            tokens: max,
            last: now,
        });
        let Rate::Bucket { tokens, last } = rate else {
            return true;
        };
        let t = tokens
            .saturating_add(now.duration_since(*last).as_nanos() as u64)
            .min(max);
        *last = now;
        *tokens = t.checked_sub(cost).unwrap_or(t);
        t >= cost
    }

    /// `quota` (`nft_quota_do_eval`): the packet's bytes are counted (from `NFTA_QUOTA_CONSUMED`);
    /// it matches until the count reaches the quota, or only from then on with `NFT_QUOTA_F_INV`
    /// (`quota over`).
    fn quota(&mut self, a: &[(u16, &[u8])]) -> bool {
        let Some(quota) = get_be64(a, 1) else {
            return false;
        };
        let invert = get_be32(a, 2).unwrap_or(0) & 1 != 0;
        let len = (self.nh.len() + self.th.len()) as u64;
        let Some(rates) = &self.rates else {
            return !invert;
        };
        let mut rates = rates.borrow_mut();
        let rate = rates
            .entry(self.at)
            .or_insert(Rate::Used(get_be64(a, 3).unwrap_or(0)));
        let Rate::Used(used) = rate else {
            return !invert;
        };
        *used = used.saturating_add(len);
        (*used >= quota) == invert
    }

    fn xt_match(&mut self, a: &[(u16, &[u8])]) -> bool {
        let (Some(name), Some(info)) = (get_str(a, 1), get(a, 3)) else {
            return false;
        };
        let rev = get_be32(a, 2).unwrap_or(0);
        let ne16 = |at: usize| {
            info.get(at..at + 2)
                .map(|x| u16::from_ne_bytes([x[0], x[1]]))
        };
        let ne32 = |at: usize| {
            info.get(at..at + 4)
                .map(|x| u32::from_ne_bytes(x.try_into().unwrap()))
        };
        match name {
            b"comment" => true,
            // xt_rateinfo (limit_mt): avg, the average time between packets in 1/10000 s, and
            // burst; a bucket of avg × burst, each packet taking avg
            b"limit" => {
                let (Some(avg), Some(burst)) = (ne32(0), ne32(4)) else {
                    return false;
                };
                let cost = u64::from(avg) * 100_000;
                self.take_tokens(cost, cost.saturating_mul(u64::from(burst)))
            }
            // xt_mark_mtinfo1: mark, mask and invert; revision 0's xt_mark_info is two unsigned
            // longs
            b"mark" => {
                let ne64 = |at: usize| {
                    info.get(at..at + 8)
                        .map(|x| u64::from_ne_bytes(x.try_into().unwrap()) as u32)
                };
                let (want, mask, inv) = if rev == 0 {
                    (ne64(0), ne64(8), info.get(16))
                } else {
                    (ne32(0), ne32(4), info.get(8))
                };
                let (Some(want), Some(mask), Some(&inv)) = (want, mask, inv) else {
                    return false;
                };
                ((self.mark & mask) == want) != (inv != 0)
            }
            // xt_owner_match_info (owner_mt in xt_owner.c): uid and gid ranges, and the
            // XT_OWNER_UID, GID and SOCKET bits of match and invert. Only locally sent packets
            // (OUTPUT) have a socket; without a socket it matches only when nothing is required.
            // Supplementary groups (XT_OWNER_SUPPL_GROUPS) are not checked
            b"owner" => {
                let (Some(umin), Some(umax), Some(gmin), Some(gmax)) =
                    (ne32(0), ne32(4), ne32(8), ne32(12))
                else {
                    return false;
                };
                let (Some(&want), Some(&inv)) = (info.get(16), info.get(17)) else {
                    return false;
                };
                const UID: u8 = 1;
                const GID: u8 = 2;
                const SOCKET: u8 = 4;
                if self.p.hook != HOOK_LOCAL_OUT {
                    return want ^ inv == 0;
                }
                if want & inv & SOCKET != 0 {
                    return false;
                }
                let uid_ok = (umin..=umax).contains(&self.p.uid) != (inv & UID != 0);
                let gid_ok = (gmin..=gmax).contains(&self.p.gid) != (inv & GID != 0);
                (want & UID == 0 || uid_ok) && (want & GID == 0 || gid_ok)
            }
            // Connection tracking state: in xt_conntrack_mtinfo1..3, match_flags and invert_flags
            // are at 146 and 148, and state_mask at 150 (one byte in revision 1); only
            // XT_CONNTRACK_STATE (and STATE_ALIAS) are recognized
            b"conntrack" => {
                let (Some(flags), Some(inv)) = (ne16(146), ne16(148)) else {
                    return false;
                };
                if flags & !(1 | 0x2000) != 0 {
                    return false;
                }
                let mask = if rev == 1 {
                    info.get(150).map(|&b| u16::from(b))
                } else {
                    ne16(150)
                };
                let Some(mask) = mask else {
                    return false;
                };
                flags & 1 == 0 || ((u32::from(mask) & self.p.ct_state != 0) != (inv & 1 != 0))
            }
            // xt_state_info: statemask
            b"state" => ne32(0).is_some_and(|m| m & self.p.ct_state != 0),
            b"addrtype" => {
                let (Some(src), Some(dst)) = (ne16(0), ne16(2)) else {
                    return false;
                };
                // rev 0: two u32s, invert_source and invert_dest; rev 1: bits 1 and 2 of flags
                let (inv_s, inv_d) = if rev == 0 {
                    (ne32(4).unwrap_or(0) != 0, ne32(8).unwrap_or(0) != 0)
                } else {
                    let f = ne32(4).unwrap_or(0);
                    (f & 1 != 0, f & 2 != 0)
                };
                let ok = |mask: u16, addr: &[u8], inv: bool| {
                    mask == 0 || ((mask & (1 << (self.p.addr_type)(addr))) != 0) != inv
                };
                ok(src, self.p.src, inv_s) && ok(dst, self.p.dst, inv_d)
            }
            b"tcp" | b"udp" => {
                let want = if name == b"tcp" {
                    IPPROTO_TCP
                } else {
                    IPPROTO_UDP
                };
                let (Some(s0), Some(s1), Some(d0), Some(d1)) = (ne16(0), ne16(2), ne16(4), ne16(6))
                else {
                    return false;
                };
                let inv = info
                    .get(if want == IPPROTO_TCP { 11 } else { 8 })
                    .copied()
                    .unwrap_or(0);
                let sp = (s0..=s1).contains(&self.p.sport) != (inv & 1 != 0);
                let dp = (d0..=d1).contains(&self.p.dport) != (inv & 2 != 0);
                let flags = want != IPPROTO_TCP || {
                    let (mask, cmp) = (
                        info.get(9).copied().unwrap_or(0),
                        info.get(10).copied().unwrap_or(0),
                    );
                    ((0x02 & mask) == cmp) != (inv & 4 != 0)
                };
                self.p.proto == want && sp && dp && flags
            }
            _ => false,
        }
    }

    /// The address redirect rewrites to: the loopback address on OUTPUT, the input interface's
    /// address on PREROUTING (`nf_nat_redirect_ipv4`; unchanged when there is no address).
    fn redirect_addr(&self) -> Vec<u8> {
        match &self.p.iif {
            Some(i) if i.addr.len() == self.p.dst.len() => i.addr.clone(),
            Some(_) => self.p.dst.to_vec(),
            None if self.p.dst.len() == 4 => vec![127, 0, 0, 1],
            None => std::net::Ipv6Addr::LOCALHOST.octets().to_vec(),
        }
    }

    fn set_dnat(&mut self, addr: Option<Vec<u8>>, port: Option<u16>) -> bool {
        let addr = addr.unwrap_or_else(|| self.p.dst.to_vec());
        self.dnat = Some((addr, port));
        self.verdict = NF_ACCEPT;
        true
    }

    fn load(&self, r: u32, len: usize) -> Option<&[u8]> {
        Some(&self.regs[reg(r, len)?])
    }

    fn store_u32(&mut self, r: u32, v: u32) -> bool {
        self.store(r, &v.to_ne_bytes())
    }

    /// Writes a register, zero-padding the part short of 4 bytes (as `nft_reg_store*` and payload
    /// writes do).
    fn store(&mut self, r: u32, data: &[u8]) -> bool {
        let Some(at) = reg(r, data.len().next_multiple_of(4)) else {
            return false;
        };
        self.regs[at.clone()].fill(0);
        self.regs[at.start..at.start + data.len()].copy_from_slice(data);
        true
    }
}

/// The error code the initiator gets from a reject expression (`NFTA_REJECT_TYPE`,
/// `NFTA_REJECT_ICMP_CODE`; `icmp_err_convert`, `icmpv6_err_convert`): TCP RST is ECONNREFUSED;
/// the ICMPX codes no route, port unreachable, host unreachable and administratively prohibited;
/// ICMP and ICMPv6 by their own codes.
fn reject_errno(ty: u32, code: u8, v6: bool) -> i32 {
    let e = match (ty, code, v6) {
        (1, _, _) => Errno::ECONNREFUSED,
        (2, 0, _) => Errno::ENETUNREACH,
        (2, 1, _) => Errno::ECONNREFUSED,
        (2, 2 | 3, _) => Errno::EHOSTUNREACH,
        (0, 0 | 6 | 9 | 11, false) => Errno::ENETUNREACH,
        (0, 2, false) => Errno::ENOPROTOOPT,
        (0, 3, false) => Errno::ECONNREFUSED,
        (0, _, false) => Errno::EHOSTUNREACH,
        (0, 0, true) => Errno::ENETUNREACH,
        (0, 1, true) => Errno::EACCES,
        (0, 4, true) => Errno::ECONNREFUSED,
        (0, _, true) => Errno::EHOSTUNREACH,
        _ => Errno::ECONNREFUSED,
    };
    e.0
}

/// The element of the set with key `key`: interval sets are searched by interval (those with
/// KEY_END are [start, end]; those without are sorted endpoints, and the last one not greater
/// than the key must not be an interval end).
fn set_find(set: &Set, key: &[u8]) -> Option<Vec<u8>> {
    if set.flags & NFT_SET_INTERVAL == 0 {
        return set
            .elems
            .iter()
            .find(|e| elem_key(e).as_deref() == Some(key))
            .cloned();
    }
    let mut best: Option<(Vec<u8>, &Vec<u8>)> = None;
    for e in &set.elems {
        let Some(k) = elem_key(e) else { continue };
        if let Some(end) = elem_key_end(e) {
            if &k[..] <= key && key <= &end[..] {
                return Some(e.clone());
            }
            continue;
        }
        if &k[..] > key {
            continue;
        }
        let better = match &best {
            None => true,
            Some((bk, be)) => {
                k > *bk || (k == *bk && elem_flags(be) & NFT_SET_ELEM_INTERVAL_END != 0)
            }
        };
        if better {
            best = Some((k, e));
        }
    }
    best.filter(|(_, e)| elem_flags(e) & NFT_SET_ELEM_INTERVAL_END == 0)
        .map(|(_, e)| e.clone())
}

/// Position of a register in [`Eval::regs`] (`nft_parse_register`): NFT_REG_1..4 are 16 bytes,
/// and NFT_REG32_00..15 are 4 bytes, overlapping the former.
fn reg(r: u32, len: usize) -> Option<std::ops::Range<usize>> {
    let at = match r {
        1..=4 => r as usize * 16,
        8..=23 => (r as usize - 4) * 4,
        _ => return None,
    };
    (at + len <= 80).then_some(at..at + len)
}

// ---- Encoding ----

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

fn get<'a>(attrs: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    attrs.iter().find(|(t, _)| *t == ty).map(|(_, d)| *d)
}

fn get_be32(attrs: &[(u16, &[u8])], ty: u16) -> Option<u32> {
    get(attrs, ty)
        .and_then(|d| d.get(..4))
        .map(|d| u32::from_be_bytes(d.try_into().unwrap()))
}

fn get_be64(attrs: &[(u16, &[u8])], ty: u16) -> Option<u64> {
    get(attrs, ty)
        .and_then(|d| d.get(..8))
        .map(|d| u64::from_be_bytes(d.try_into().unwrap()))
}

/// A string attribute (without the terminating NUL).
fn get_str<'a>(attrs: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    get(attrs, ty).map(|d| d.split(|&c| c == 0).next().unwrap_or_default())
}

fn cstr(s: &[u8]) -> Vec<u8> {
    let mut v = s.to_vec();
    v.push(0);
    v
}

/// One nfnetlink message: nfgenmsg (address family, version, `res_id`) plus attributes.
fn nfmsg(ty: u16, flags: u16, seq: u32, pid: u32, family: u8, res: u16, attrs: &[u8]) -> Vec<u8> {
    let mut p = vec![family, 0];
    p.extend_from_slice(&res.to_be_bytes());
    p.extend_from_slice(attrs);
    message(ty, flags, seq, pid, &p)
}

fn nft_type(msg: u8) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | u16::from(msg)
}

fn done(seq: u32, pid: u32) -> Vec<u8> {
    message(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes())
}

/// Removes `NLA_F_NESTED` from nested attributes (user space sets it; 6.1 kernels leave it out of
/// dumps).
fn unflag(mut b: &[u8]) -> Vec<u8> {
    const NLA_F_NESTED: u16 = 0x8000;
    let mut out = Vec::new();
    while b.len() >= 4 {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        if len < 4 || len > b.len() {
            break;
        }
        let ty = u16::from_le_bytes([b[2], b[3]]);
        if ty & NLA_F_NESTED != 0 {
            attr(&mut out, ty & 0x3fff, &unflag(&b[4..len]));
        } else {
            attr(&mut out, ty & 0x3fff, &b[4..len]);
        }
        b = &b[len.next_multiple_of(4).min(b.len())..];
    }
    out
}

/// A rule's expressions: (name, arguments).
fn exprs(e: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    crate::route::attrs(e)
        .into_iter()
        .map(|(_, x)| {
            let a = crate::route::attrs(x);
            (
                get_str(&a, EXPR_NAME).unwrap_or_default().to_vec(),
                get(&a, EXPR_DATA).unwrap_or_default().to_vec(),
            )
        })
        .collect()
}

/// An element's key (the value in `NFTA_SET_ELEM_KEY`).
fn elem_key(e: &[u8]) -> Option<Vec<u8>> {
    let a = crate::route::attrs(e);
    data_value(get(&a, SET_ELEM_KEY)?)
}

fn elem_key_end(e: &[u8]) -> Option<Vec<u8>> {
    let a = crate::route::attrs(e);
    data_value(get(&a, SET_ELEM_KEY_END)?)
}

fn elem_flags(e: &[u8]) -> u32 {
    get_be32(&crate::route::attrs(e), SET_ELEM_FLAGS).unwrap_or(0)
}

/// The value in `NFTA_DATA_VALUE`.
fn data_value(d: &[u8]) -> Option<Vec<u8>> {
    get(&crate::route::attrs(d), DATA_VALUE).map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::RefCell;

    const NLM_F_CREATE: u16 = 0x400;

    fn ns() -> NetNs {
        NetNs {
            id: 1,
            owner: crate::cred::UserNs::init(),
            host_net: Some([3, 4]),
            lo: Default::default(),
            devs: Default::default(),
            nft: RefCell::new(Nft::default()),
            listeners: Default::default(),
        }
    }

    fn peer() -> Peer<'static> {
        Peer {
            pid: 77,
            ext_ack: false,
            cap_ack: false,
            proc_pid: 42,
            comm: b"iptables",
        }
    }

    fn a(b: &mut Vec<u8>, ty: u16, d: &[u8]) {
        attr(b, ty, d);
    }

    fn value(v: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        a(&mut d, DATA_VALUE, v);
        d
    }

    fn verdict(code: i32, chain: Option<&[u8]>) -> Vec<u8> {
        let mut v = Vec::new();
        a(&mut v, VERDICT_CODE, &be32(code as u32));
        if let Some(c) = chain {
            a(&mut v, VERDICT_CHAIN, &cstr(c));
        }
        let mut d = Vec::new();
        a(&mut d, DATA_VERDICT, &v);
        d
    }

    /// An expression (NFTA_LIST_ELEM): the arguments are (attribute, contents).
    fn expr(name: &str, args: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut d = Vec::new();
        for (t, v) in args {
            a(&mut d, *t, v);
        }
        let mut e = Vec::new();
        a(&mut e, EXPR_NAME, &cstr(name.as_bytes()));
        a(&mut e, EXPR_DATA, &d);
        let mut l = Vec::new();
        // User space sets NLA_F_NESTED
        a(&mut l, LIST_ELEM | 0x8000, &e);
        l
    }

    fn payload(base: u32, off: u32, len: u32) -> Vec<u8> {
        expr(
            "payload",
            &[
                (1, be32(1).to_vec()),
                (2, be32(base).to_vec()),
                (3, be32(off).to_vec()),
                (4, be32(len).to_vec()),
            ],
        )
    }

    fn cmp_eq(v: &[u8]) -> Vec<u8> {
        expr(
            "cmp",
            &[(1, be32(1).to_vec()), (2, be32(0).to_vec()), (3, value(v))],
        )
    }

    fn jump(chain: &[u8]) -> Vec<u8> {
        expr(
            "immediate",
            &[(1, be32(0).to_vec()), (2, verdict(NFT_JUMP, Some(chain)))],
        )
    }

    fn msg(ty: u8, flags: u16, family: u8, attrs: &[u8]) -> Vec<u8> {
        nfmsg(nft_type(ty), flags | NLM_F_REQUEST, 1, 0, family, 0, attrs)
    }

    fn table_msg(family: u8, name: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        a(&mut p, TABLE_NAME, &cstr(name));
        msg(NEWTABLE, NLM_F_CREATE, family, &p)
    }

    fn chain_msg(
        family: u8,
        table: &[u8],
        name: &[u8],
        hook: Option<(u32, i32, &[u8])>,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        a(&mut p, CHAIN_TABLE, &cstr(table));
        a(&mut p, CHAIN_NAME, &cstr(name));
        if let Some((num, prio, ty)) = hook {
            let mut h = Vec::new();
            a(&mut h, HOOK_HOOKNUM, &be32(num));
            a(&mut h, HOOK_PRIORITY, &be32(prio as u32));
            a(&mut p, CHAIN_HOOK | 0x8000, &h);
            a(&mut p, CHAIN_POLICY, &be32(1));
            a(&mut p, CHAIN_TYPE, &cstr(ty));
        }
        msg(NEWCHAIN, NLM_F_CREATE, family, &p)
    }

    fn rule_msg(family: u8, table: &[u8], chain: &[u8], exprs: &[Vec<u8>]) -> Vec<u8> {
        let mut p = Vec::new();
        a(&mut p, RULE_TABLE, &cstr(table));
        a(&mut p, RULE_CHAIN, &cstr(chain));
        a(&mut p, RULE_EXPRESSIONS | 0x8000, &exprs.concat());
        msg(NEWRULE, NLM_F_CREATE | NLM_F_APPEND, family, &p)
    }

    fn batch(msgs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = message(NFNL_MSG_BATCH_BEGIN, NLM_F_REQUEST, 0, 0, &[0, 0, 0, 10]);
        for m in msgs {
            b.extend_from_slice(m);
        }
        b.extend(message(
            NFNL_MSG_BATCH_END,
            NLM_F_REQUEST,
            9,
            0,
            &[0, 0, 0, 10],
        ));
        b
    }

    /// Error codes in the answers (of NLMSG_ERROR; 0 is an acknowledgement), in order.
    fn errors(replies: &[Vec<u8>]) -> Vec<i32> {
        replies
            .iter()
            .flat_map(|d| split(d))
            .filter(|m| u16::from_le_bytes([m[4], m[5]]) == 2)
            .map(|m| -i32::from_le_bytes(m[16..20].try_into().unwrap()))
            .collect()
    }

    /// Attributes of each message in a dump answer (excluding DONE).
    fn dumped(replies: &[Vec<u8>]) -> Vec<Vec<(u16, Vec<u8>)>> {
        replies
            .iter()
            .flat_map(|d| split(d))
            .filter(|m| u16::from_le_bytes([m[4], m[5]]) >> 8 == NFNL_SUBSYS_NFTABLES)
            .map(|m| {
                crate::route::attrs(&m[20..])
                    .into_iter()
                    .map(|(t, d)| (t, d.to_vec()))
                    .collect()
            })
            .collect()
    }

    fn find(attrs: &[(u16, Vec<u8>)], ty: u16) -> &[u8] {
        &attrs.iter().find(|(t, _)| *t == ty).unwrap().1
    }

    fn generation(ns: &NetNs) -> u32 {
        let r = handle(ns, &Cred::root(), &msg(GETGEN, 0, 0, &[]), &peer());
        let m = &dumped(&r)[0];
        u32::from_be_bytes(find(m, GEN_ID).try_into().unwrap())
    }

    fn packet<'a>(
        proto: u8,
        dst: &'a [u8],
        dport: u16,
        local: &'a dyn Fn(&[u8]) -> u32,
    ) -> Packet<'a> {
        Packet {
            src: &[127, 0, 0, 1],
            dst,
            proto,
            sport: 33000,
            dport,
            uid: 0,
            gid: 0,
            addr_type: local,
            oif: &|| (1, b"lo".to_vec()),
            hook: HOOK_LOCAL_OUT,
            iif: None,
            ct_state: CT_NEW,
            mark: 0,
        }
    }

    /// Whether connection tracking is on ([`Nft::tracks`]): an empty nat chain does not turn it
    /// on; ct, redirect and xt REDIRECT do; ip and ip6 tables each cover their own family, and
    /// inet tables cover both.
    #[test]
    fn conntrack_follows_the_rules() {
        let root = Cred::root();
        let run = |msgs: &[Vec<u8>]| {
            let ns = ns();
            let r = handle(&ns, &root, &batch(msgs), &peer());
            assert!(errors(&r).iter().all(|&e| e == 0), "{:?}", errors(&r));
            let n = ns.nft.borrow();
            (n.tracks(false), n.tracks(true))
        };
        let empty = |fam: u8| {
            vec![
                table_msg(fam, b"t"),
                chain_msg(fam, b"t", b"out", Some((3, -100, b"nat"))),
            ]
        };
        assert_eq!(run(&empty(2)), (false, false));
        let with = |fam: u8, e: Vec<u8>| {
            let mut m = empty(fam);
            m.push(rule_msg(fam, b"t", b"out", &[e]));
            m
        };
        let redir = || expr("redir", &[(1, be32(1).to_vec())]);
        assert_eq!(run(&with(2, redir())), (true, false));
        assert_eq!(run(&with(10, redir())), (false, true));
        assert_eq!(run(&with(1, redir())), (true, true));
        let ct = expr("ct", &[(1, be32(1).to_vec()), (2, be32(0).to_vec())]);
        assert_eq!(run(&with(2, ct)), (true, false));
        // Other expressions do not turn it on
        assert_eq!(run(&with(2, expr("counter", &[]))), (false, false));
        let xt = |name: &[u8]| {
            expr(
                "target",
                &[(1, cstr(name)), (2, be32(0).to_vec()), (3, vec![0; 20])],
            )
        };
        assert_eq!(run(&with(2, xt(b"REDIRECT"))), (true, false));
        assert_eq!(run(&with(2, xt(b"LOG"))), (false, false));
    }

    /// xt mark and owner matches (older iptables-nft sends `-m mark` as xt, current ones translate
    /// it to meta mark; `-m owner` always goes through xt): marks are compared under the mask,
    /// owner checks uid and gid only on locally sent packets, and packets without a socket match
    /// only when nothing is required (`! --uid-owner` matches).
    #[test]
    fn xt_mark_and_owner() {
        let ns = ns();
        let root = Cred::root();
        let dnat_to = |ip: [u8; 4]| {
            let mut d = vec![0u8; 48];
            d[..4].copy_from_slice(&3u32.to_ne_bytes());
            d[4..8].copy_from_slice(&ip);
            d[20..24].copy_from_slice(&ip);
            d[36..38].copy_from_slice(&80u16.to_be_bytes());
            d[38..40].copy_from_slice(&80u16.to_be_bytes());
            expr(
                "target",
                &[(1, cstr(b"DNAT")), (2, be32(2).to_vec()), (3, d)],
            )
        };
        let xt = |name: &[u8], rev: u32, info: Vec<u8>| {
            expr(
                "match",
                &[(1, cstr(name)), (2, be32(rev).to_vec()), (3, info)],
            )
        };
        let mut mark = Vec::new();
        mark.extend_from_slice(&7u32.to_ne_bytes());
        mark.extend_from_slice(&0xffu32.to_ne_bytes());
        mark.extend_from_slice(&[0; 4]);
        // uid 1000, gid 50..60, match and invert
        let owner = |want: u8, inv: u8| {
            let mut o = Vec::new();
            for v in [1000u32, 1000, 50, 60] {
                o.extend_from_slice(&v.to_ne_bytes());
            }
            o.extend_from_slice(&[want, inv, 0, 0]);
            o
        };
        let r = handle(
            &ns,
            &root,
            &batch(&[
                table_msg(2, b"nat"),
                chain_msg(2, b"nat", b"OUTPUT", Some((3, -100, b"nat"))),
                chain_msg(2, b"nat", b"PREROUTING", Some((0, -100, b"nat"))),
                rule_msg(
                    2,
                    b"nat",
                    b"OUTPUT",
                    &[xt(b"mark", 1, mark), dnat_to([10, 0, 0, 1])],
                ),
                rule_msg(
                    2,
                    b"nat",
                    b"OUTPUT",
                    &[xt(b"owner", 1, owner(1, 0)), dnat_to([10, 0, 0, 2])],
                ),
                rule_msg(
                    2,
                    b"nat",
                    b"OUTPUT",
                    &[xt(b"owner", 1, owner(2, 2)), dnat_to([10, 0, 0, 3])],
                ),
                rule_msg(
                    2,
                    b"nat",
                    b"PREROUTING",
                    &[xt(b"owner", 1, owner(1, 0)), dnat_to([10, 0, 0, 4])],
                ),
                rule_msg(
                    2,
                    b"nat",
                    b"PREROUTING",
                    &[xt(b"owner", 1, owner(1, 1)), dnat_to([10, 0, 0, 5])],
                ),
            ]),
            &peer(),
        );
        assert!(errors(&r).iter().all(|&e| e == 0), "{:?}", errors(&r));
        let unicast = |_: &[u8]| RTN_UNICAST;
        let st = ns.nft.borrow();
        let dst = [127, 0, 0, 20];
        let to = |ip: [u8; 4]| Outcome::Dnat(ip.to_vec(), Some(80));
        let run = |mark: u32, uid: u32, gid: u32, hook: u32| {
            let mut p = packet(6, &dst, 2290, &unicast);
            (p.mark, p.uid, p.gid, p.hook) = (mark, uid, gid, hook);
            st.nat(&p)
        };
        // Marks under the mask: 0x107 & 0xff is 7 too
        assert_eq!(run(7, 0, 55, HOOK_LOCAL_OUT), to([10, 0, 0, 1]));
        assert_eq!(run(0x107, 0, 55, HOOK_LOCAL_OUT), to([10, 0, 0, 1]));
        // uid 1000
        assert_eq!(run(8, 1000, 55, HOOK_LOCAL_OUT), to([10, 0, 0, 2]));
        // gid not in 50..60 (inverted)
        assert_eq!(run(0, 0, 61, HOOK_LOCAL_OUT), to([10, 0, 0, 3]));
        assert_eq!(run(0, 0, 55, HOOK_LOCAL_OUT), Outcome::Accept);
        // No socket on PREROUTING: --uid-owner does not match, ! --uid-owner does
        assert_eq!(run(0, 1000, 55, HOOK_PRE_ROUTING), to([10, 0, 0, 5]));
    }

    /// The built-in DNS rules docker writes into containers (as iptables-nft sends them,
    /// docs/rounds/172-nftables.md).
    #[test]
    fn iptables_dnat_rules() {
        let ns = ns();
        let root = Cred::root();
        assert_eq!(generation(&ns), 1);
        let mut dnat = vec![0u8; 48];
        dnat[..4].copy_from_slice(&3u32.to_ne_bytes());
        dnat[4..8].copy_from_slice(&[127, 0, 0, 11]);
        dnat[20..24].copy_from_slice(&[127, 0, 0, 11]);
        dnat[36..38].copy_from_slice(&40001u16.to_be_bytes());
        dnat[38..40].copy_from_slice(&40001u16.to_be_bytes());
        let target = expr(
            "target",
            &[(1, cstr(b"DNAT")), (2, be32(2).to_vec()), (3, dnat)],
        );
        let r = handle(
            &ns,
            &root,
            &batch(&[
                table_msg(2, b"nat"),
                chain_msg(2, b"nat", b"DOCKER_OUTPUT", None),
                chain_msg(2, b"nat", b"OUTPUT", Some((3, -100, b"nat"))),
                rule_msg(
                    2,
                    b"nat",
                    b"OUTPUT",
                    &[
                        payload(1, 16, 4),
                        cmp_eq(&[127, 0, 0, 11]),
                        expr("counter", &[]),
                        jump(b"DOCKER_OUTPUT"),
                    ],
                ),
                rule_msg(
                    2,
                    b"nat",
                    b"DOCKER_OUTPUT",
                    &[
                        payload(1, 16, 4),
                        cmp_eq(&[127, 0, 0, 11]),
                        payload(1, 9, 1),
                        cmp_eq(&[17]),
                        payload(2, 2, 2),
                        cmp_eq(&[0, 53]),
                        target,
                    ],
                ),
            ]),
            &peer(),
        );
        // No acknowledgement requested: nothing is answered on success
        assert!(r.is_empty(), "{r:?}");
        assert_eq!(generation(&ns), 2);

        // Chain dump: handles come from the table's counter, and USE is the number of rules plus
        // jumps to it
        let r = handle(&ns, &root, &msg(GETCHAIN, NLM_F_DUMP, 2, &[]), &peer());
        let chains = dumped(&r);
        assert_eq!(chains.len(), 2);
        assert_eq!(find(&chains[0], CHAIN_NAME), b"DOCKER_OUTPUT\0");
        assert_eq!(find(&chains[0], CHAIN_HANDLE), be64(1));
        assert_eq!(find(&chains[0], CHAIN_USE), be32(2));
        assert_eq!(find(&chains[1], CHAIN_USE), be32(1));
        assert_eq!(find(&chains[1], CHAIN_FLAGS), be32(NFT_CHAIN_BASE));
        // A table's USE is the number of chains
        let r = handle(&ns, &root, &msg(GETTABLE, NLM_F_DUMP, 0, &[]), &peer());
        assert_eq!(find(&dumped(&r)[0], TABLE_USE), be32(2));
        // Rules come back as given (without NLA_F_NESTED), filtered by TABLE and CHAIN
        let mut p = Vec::new();
        a(&mut p, RULE_TABLE, &cstr(b"nat"));
        a(&mut p, RULE_CHAIN, &cstr(b"OUTPUT"));
        let r = handle(&ns, &root, &msg(GETRULE, NLM_F_DUMP, 2, &p), &peer());
        let rules = dumped(&r);
        assert_eq!(rules.len(), 1);
        assert_eq!(find(&rules[0], RULE_HANDLE), be64(3));
        assert_eq!(
            exprs(find(&rules[0], RULE_EXPRESSIONS))
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>(),
            [&b"payload"[..], b"cmp", b"counter", b"immediate"]
        );
        let e = find(&rules[0], RULE_EXPRESSIONS);
        assert_eq!(u16::from_le_bytes([e[2], e[3]]), LIST_ELEM);

        // Execution: only UDP to 127.0.0.11:53 gets a new destination address
        let unicast = |_: &[u8]| RTN_UNICAST;
        let st = ns.nft.borrow();
        let dns = [127, 0, 0, 11];
        assert_eq!(
            st.nat(&packet(17, &dns, 53, &unicast)),
            Outcome::Dnat(dns.to_vec(), Some(40001))
        );
        assert_eq!(st.nat(&packet(6, &dns, 53, &unicast)), Outcome::Accept);
        assert_eq!(
            st.nat(&packet(17, &[127, 0, 0, 12], 53, &unicast)),
            Outcome::Accept
        );
        drop(st);

        // A failed batch is discarded as a whole: a jump to a missing chain (ENOENT), generation
        // number unchanged
        let r = handle(
            &ns,
            &root,
            &batch(&[
                chain_msg(2, b"nat", b"X", None),
                rule_msg(2, b"nat", b"X", &[jump(b"NOPE")]),
            ]),
            &peer(),
        );
        assert_eq!(errors(&r), [Errno::ENOENT.0]);
        assert_eq!(generation(&ns), 2);
        assert_eq!(ns.nft.borrow().tables[0].chains.len(), 2);
        // A chain jumped to cannot be deleted (EBUSY); jumping back to itself is a loop (EMLINK);
        // jumping to a base chain is not allowed
        let mut p = Vec::new();
        a(&mut p, CHAIN_TABLE, &cstr(b"nat"));
        a(&mut p, CHAIN_NAME, &cstr(b"DOCKER_OUTPUT"));
        let del = msg(DELCHAIN, 0, 2, &p);
        let r = handle(&ns, &root, &batch(std::slice::from_ref(&del)), &peer());
        assert_eq!(errors(&r), [Errno::EBUSY.0]);
        // With NETLINK_EXT_ACK set: the offset in the request of the offending attribute (the
        // chain name) is attached
        let ext = Peer {
            ext_ack: true,
            ..peer()
        };
        let r = handle(&ns, &root, &batch(std::slice::from_ref(&del)), &ext);
        let m = split(&r[0])[0];
        assert_eq!(u16::from_le_bytes([m[6], m[7]]) & 0x200, 0x200);
        let tlv = crate::route::attrs(&m[20 + del.len()..]);
        // After nfgenmsg: CHAIN_TABLE ("nat", 8 bytes), CHAIN_NAME
        assert_eq!(get(&tlv, 2), Some(&(16u32 + 4 + 8).to_ne_bytes()[..]));
        let r = handle(
            &ns,
            &root,
            &batch(&[
                chain_msg(2, b"nat", b"A", None),
                chain_msg(2, b"nat", b"B", None),
                rule_msg(2, b"nat", b"A", &[jump(b"B")]),
                rule_msg(2, b"nat", b"B", &[jump(b"A")]),
                rule_msg(2, b"nat", b"A", &[jump(b"OUTPUT")]),
            ]),
            &peer(),
        );
        assert_eq!(errors(&r), [Errno::EMLINK.0, Errno::EOPNOTSUPP.0]);
        // The generation number at the start of the batch is out of date: ERESTART
        let mut begin = Vec::new();
        a(&mut begin, NFNL_BATCH_GENID, &be32(1));
        let mut b = message(
            NFNL_MSG_BATCH_BEGIN,
            NLM_F_REQUEST,
            0,
            0,
            &[[0, 0, 0, 10].as_slice(), &begin].concat(),
        );
        b.extend(message(
            NFNL_MSG_BATCH_END,
            NLM_F_REQUEST,
            9,
            0,
            &[0, 0, 0, 10],
        ));
        assert_eq!(errors(&handle(&ns, &root, &b, &peer())), [ERESTART]);
        // Without CAP_NET_ADMIN: EPERM, for queries too
        let user = Cred {
            euid: 1000,
            cap_effective: 0,
            cap_permitted: 0,
            ..Cred::root()
        };
        assert_eq!(
            errors(&handle(&ns, &user, &msg(GETGEN, 0, 0, &[]), &peer())),
            [Errno::EPERM.0]
        );
        // A single query asking for an acknowledgement: the answer, then the acknowledgement;
        // not found is ENOENT
        let mut p = Vec::new();
        a(&mut p, CHAIN_TABLE, &cstr(b"nat"));
        a(&mut p, CHAIN_NAME, &cstr(b"INPUT"));
        assert_eq!(
            errors(&handle(
                &ns,
                &root,
                &msg(GETCHAIN, NLM_F_ACK, 2, &p),
                &peer()
            )),
            [Errno::ENOENT.0]
        );
        let mut p = Vec::new();
        a(&mut p, CHAIN_TABLE, &cstr(b"nat"));
        a(&mut p, CHAIN_NAME, &cstr(b"OUTPUT"));
        let r = handle(&ns, &root, &msg(GETCHAIN, NLM_F_ACK, 2, &p), &peer());
        assert_eq!(dumped(&r).len(), 1);
        assert_eq!(errors(&r), [0]);
    }

    /// Port forwarding by netavark's nftables driver (inet table, anonymous sets, fib, nftables'
    /// own dnat).
    #[test]
    fn netavark_hostport_dnat() {
        let ns = ns();
        let root = Cred::root();
        let mut set = Vec::new();
        a(&mut set, SET_TABLE, &cstr(b"netavark"));
        a(&mut set, SET_NAME, &cstr(b"__set%d"));
        a(&mut set, SET_FLAGS, &be32(3));
        a(&mut set, SET_KEY_TYPE, &be32(12));
        a(&mut set, SET_KEY_LEN, &be32(1));
        a(&mut set, SET_ID, &be32(1));
        let mut elems = Vec::new();
        for proto in [6u8, 17] {
            let mut k = Vec::new();
            a(&mut k, SET_ELEM_KEY | 0x8000, &value(&[proto]));
            a(&mut elems, LIST_ELEM | 0x8000, &k);
        }
        let mut se = Vec::new();
        a(&mut se, SET_ELEM_LIST_TABLE, &cstr(b"netavark"));
        a(&mut se, SET_ELEM_LIST_SET, &cstr(b"__set%d"));
        a(&mut se, SET_ELEM_LIST_SET_ID, &be32(1));
        a(&mut se, SET_ELEM_LIST_ELEMENTS | 0x8000, &elems);
        let reg = |r: u32| be32(r).to_vec();
        let r = handle(
            &ns,
            &root,
            &batch(&[
                table_msg(1, b"netavark"),
                chain_msg(1, b"netavark", b"OUTPUT", Some((3, -100, b"nat"))),
                chain_msg(1, b"netavark", b"HOSTPORT-DNAT", None),
                chain_msg(1, b"netavark", b"dnat", None),
                msg(NEWSET, NLM_F_CREATE, 1, &set),
                msg(NEWSETELEM, NLM_F_CREATE, 1, &se),
                rule_msg(
                    1,
                    b"netavark",
                    b"OUTPUT",
                    &[
                        expr("fib", &[(1, reg(1)), (2, reg(3)), (3, reg(2))]),
                        cmp_eq(&RTN_LOCAL.to_ne_bytes()),
                        jump(b"HOSTPORT-DNAT"),
                    ],
                ),
                rule_msg(
                    1,
                    b"netavark",
                    b"HOSTPORT-DNAT",
                    &[
                        expr("meta", &[(2, reg(16)), (1, reg(1))]),
                        expr("lookup", &[(2, reg(1)), (1, cstr(b"__set%d")), (4, reg(1))]),
                        payload(2, 2, 2),
                        cmp_eq(&8080u16.to_be_bytes()),
                        jump(b"dnat"),
                    ],
                ),
                rule_msg(
                    1,
                    b"netavark",
                    b"dnat",
                    &[
                        expr("meta", &[(2, reg(16)), (1, reg(1))]),
                        cmp_eq(&[6]),
                        expr("immediate", &[(1, reg(1)), (2, value(&[10, 89, 0, 2]))]),
                        expr(
                            "immediate",
                            &[(1, reg(2)), (2, value(&80u16.to_be_bytes()))],
                        ),
                        expr(
                            "nat",
                            &[
                                (1, reg(1)),
                                (2, reg(2)),
                                (3, reg(1)),
                                (5, reg(2)),
                                (7, reg(2)),
                            ],
                        ),
                    ],
                ),
            ]),
            &peer(),
        );
        assert!(r.is_empty(), "{:?}", errors(&r));
        // The anonymous set got a name, and lookup refers to it by name (no SET_ID in the dump)
        let r = handle(&ns, &root, &msg(GETSET, NLM_F_DUMP, 0, &[]), &peer());
        let sets = dumped(&r);
        assert_eq!(find(&sets[0], SET_NAME), b"__set0\0");
        assert_eq!(find(&sets[0], SET_KEY_TYPE), be32(12));
        let r = handle(&ns, &root, &msg(GETRULE, NLM_F_DUMP, 1, &[]), &peer());
        let rules = dumped(&r);
        let lookup = exprs(find(&rules[1], RULE_EXPRESSIONS))
            .into_iter()
            .find(|(n, _)| n == b"lookup")
            .unwrap()
            .1;
        let la = crate::route::attrs(&lookup);
        assert_eq!(get_str(&la, LOOKUP_SET), Some(&b"__set0"[..]));
        assert!(get(&la, LOOKUP_SET_ID).is_none());
        // Set elements: one message with the elements, then an empty one and DONE
        let mut p = Vec::new();
        a(&mut p, SET_ELEM_LIST_TABLE, &cstr(b"netavark"));
        a(&mut p, SET_ELEM_LIST_SET, &cstr(b"__set0"));
        let r = handle(&ns, &root, &msg(GETSETELEM, NLM_F_DUMP, 1, &p), &peer());
        assert_eq!(r.len(), 2);
        let el = dumped(&r);
        assert_eq!(
            crate::route::attrs(find(&el[0], SET_ELEM_LIST_ELEMENTS)).len(),
            2
        );
        assert!(find(&el[1], SET_ELEM_LIST_ELEMENTS).is_empty());

        // Execution: 8080 on local addresses becomes the container's 80; other addresses and UDP
        // are unchanged
        let local = |x: &[u8]| addr_type(x, |y| y[0] == 127);
        let st = ns.nft.borrow();
        let lo = [127, 0, 0, 1];
        assert_eq!(
            st.nat(&packet(6, &lo, 8080, &local)),
            Outcome::Dnat(vec![10, 89, 0, 2], Some(80))
        );
        assert_eq!(
            st.nat(&packet(6, &[10, 1, 2, 3], 8080, &local)),
            Outcome::Accept
        );
        assert_eq!(st.nat(&packet(17, &lo, 8080, &local)), Outcome::Accept);
        assert_eq!(st.nat(&packet(6, &lo, 8081, &local)), Outcome::Accept);
        drop(st);
        // A set in use cannot be deleted; after deleting the table, nothing is left
        let mut p = Vec::new();
        a(&mut p, SET_TABLE, &cstr(b"netavark"));
        a(&mut p, SET_NAME, &cstr(b"__set0"));
        assert_eq!(
            errors(&handle(
                &ns,
                &root,
                &batch(&[msg(DELSET, 0, 1, &p)]),
                &peer()
            )),
            [Errno::EBUSY.0]
        );
        let mut p = Vec::new();
        a(&mut p, TABLE_NAME, &cstr(b"netavark"));
        assert!(handle(&ns, &root, &batch(&[msg(DELTABLE, 0, 1, &p)]), &peer()).is_empty());
        assert!(ns.nft.borrow().is_empty());
        // Deleting again is ENOENT; DESTROY of something missing still succeeds
        assert_eq!(
            errors(&handle(
                &ns,
                &root,
                &batch(&[msg(DELTABLE, 0, 1, &p)]),
                &peer()
            )),
            [Errno::ENOENT.0]
        );
        assert!(handle(&ns, &root, &batch(&[msg(DESTROYTABLE, 0, 1, &p)]), &peer()).is_empty());
    }

    /// Filter chains: drop and reject on FORWARD by input interface and port, the xt conntrack
    /// match recognizes ESTABLISHED, and the INPUT policy is drop.
    #[test]
    fn filter_chains() {
        let ns = ns();
        let root = Cred::root();
        let reg = |r: u32| be32(r).to_vec();
        let mut br0 = b"br0".to_vec();
        br0.resize(16, 0);
        let dport = |p: u16| [payload(2, 2, 2), cmp_eq(&p.to_be_bytes())];
        let verdict_expr = |code: i32| expr("immediate", &[(1, reg(0)), (2, verdict(code, None))]);
        // xt_conntrack_mtinfo3: match_flags = XT_CONNTRACK_STATE, state_mask = ESTABLISHED|RELATED
        let mut ct = vec![0u8; 168];
        ct[146..148].copy_from_slice(&1u16.to_ne_bytes());
        ct[150..152].copy_from_slice(&6u16.to_ne_bytes());
        let conntrack = expr(
            "match",
            &[(1, cstr(b"conntrack")), (2, be32(3).to_vec()), (3, ct)],
        );
        let mut input = chain_msg(1, b"t", b"INPUT", Some((1, 0, b"filter")));
        // Policy drop (chain_msg writes accept)
        let at = input
            .windows(8)
            .position(|w| w == [8, 0, CHAIN_POLICY as u8, 0, 0, 0, 0, 1])
            .unwrap();
        input[at + 7] = 0;
        let r = handle(
            &ns,
            &root,
            &batch(&[
                table_msg(1, b"t"),
                chain_msg(1, b"t", b"FORWARD", Some((2, 0, b"filter"))),
                input,
                rule_msg(1, b"t", b"FORWARD", &[conntrack, verdict_expr(NF_ACCEPT)]),
                rule_msg(
                    1,
                    b"t",
                    b"FORWARD",
                    &[
                        expr("meta", &[(2, reg(6)), (1, reg(1))]),
                        cmp_eq(&br0),
                        dport(7001)[0].clone(),
                        dport(7001)[1].clone(),
                        verdict_expr(NF_DROP),
                    ],
                ),
                rule_msg(
                    1,
                    b"t",
                    b"FORWARD",
                    &[
                        dport(7002)[0].clone(),
                        dport(7002)[1].clone(),
                        expr("reject", &[(1, be32(1).to_vec())]),
                    ],
                ),
                rule_msg(
                    1,
                    b"t",
                    b"INPUT",
                    &[
                        dport(22)[0].clone(),
                        dport(22)[1].clone(),
                        verdict_expr(NF_ACCEPT),
                    ],
                ),
            ]),
            &peer(),
        );
        assert!(r.is_empty(), "{:?}", errors(&r));
        let unicast = |_: &[u8]| RTN_UNICAST;
        let st = ns.nft.borrow();
        let dst = [10, 81, 0, 3];
        let at = |hook: u32, port: u16, iif: &[u8], ct: u32| {
            let mut p = packet(6, &dst, port, &unicast);
            p.hook = hook;
            p.iif = Some(Iif {
                index: 3,
                name: iif.to_vec(),
                addr: vec![10, 80, 0, 1],
            });
            p.ct_state = ct;
            st.filter(&p)
        };
        assert_eq!(at(2, 7000, b"br0", CT_NEW), Outcome::Accept);
        assert_eq!(at(2, 7001, b"br0", CT_NEW), Outcome::Drop);
        assert_eq!(at(2, 7001, b"br1", CT_NEW), Outcome::Accept);
        assert_eq!(at(2, 7001, b"br0", CT_ESTABLISHED), Outcome::Accept);
        assert_eq!(
            at(2, 7002, b"br1", CT_NEW),
            Outcome::Reject(Errno::ECONNREFUSED.0)
        );
        assert_eq!(at(1, 22, b"br0", CT_NEW), Outcome::Accept);
        assert_eq!(at(1, 80, b"br0", CT_NEW), Outcome::Drop);
        // No filter chains on other hooks
        assert_eq!(at(3, 7001, b"br0", CT_NEW), Outcome::Accept);
        assert!(st.has_filter(2) && st.has_filter(1) && !st.has_filter(0));
        // reject error codes
        assert_eq!(reject_errno(2, 1, false), Errno::ECONNREFUSED.0);
        assert_eq!(reject_errno(0, 1, false), Errno::EHOSTUNREACH.0);
        assert_eq!(reject_errno(0, 0, false), Errno::ENETUNREACH.0);
        assert_eq!(reject_errno(0, 4, true), Errno::ECONNREFUSED.0);
    }

    /// Interval sets: sorted endpoints, with end elements carrying NFT_SET_ELEM_INTERVAL_END;
    /// those with KEY_END are closed intervals.
    #[test]
    fn interval_sets() {
        let elem = |k: &[u8], end: bool, key_end: Option<&[u8]>| {
            let mut e = Vec::new();
            a(&mut e, SET_ELEM_KEY, &value(k));
            if let Some(x) = key_end {
                a(&mut e, SET_ELEM_KEY_END, &value(x));
            }
            if end {
                a(&mut e, SET_ELEM_FLAGS, &be32(NFT_SET_ELEM_INTERVAL_END));
            }
            e
        };
        let mut s = Set {
            name: b"s".to_vec(),
            handle: 1,
            flags: NFT_SET_INTERVAL,
            key_len: 2,
            id: None,
            other: Vec::new(),
            // 80-89, 100-100 (an end is the next value, exclusive)
            elems: vec![
                elem(&[0, 80], false, None),
                elem(&[0, 90], true, None),
                elem(&[0, 100], false, None),
                elem(&[0, 101], true, None),
            ],
        };
        let hit = |s: &Set, p: u16| set_find(s, &p.to_be_bytes()).is_some();
        assert!(!hit(&s, 79));
        assert!(hit(&s, 80) && hit(&s, 89));
        assert!(!hit(&s, 90) && !hit(&s, 99));
        assert!(hit(&s, 100) && !hit(&s, 101));
        s.elems = vec![elem(&[0, 10], false, Some(&[0, 20]))];
        assert!(hit(&s, 10) && hit(&s, 20) && !hit(&s, 21));
        s.flags = 0;
        assert!(hit(&s, 10) && !hit(&s, 11));
    }
}
