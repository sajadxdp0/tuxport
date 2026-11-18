# kernel/nft Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/nft.md` for the paired decision history (not yet created; pre-split decisions D4, D5, D8–D11 and the nftables half of D13 stay in `.agents/decisions/kernel/netns.md`).
>
> Verified: 2026-11-15 (split out of the kernel/netns full mapping of 2026-11-03; the boundary was checked against the source at the split)

## Responsibilities

`kernel/nft` owns nftables rulesets, the `NETLINK_NETFILTER` control plane and synthetic-packet
expression evaluation. Root: `core/kernel/src/nft.rs`. Namespace translation, path construction,
hook selection and forwarding remain in `kernel/netns`; socket error handling stays in `kernel/net`,
and netlink transport in `kernel/netlink`.

## Key types

- `Nft` — generation and tables; `Table`, `Chain`/`Base` (hook, priority, type, policy), `Rule`
  (raw expression attributes), `Set` (key length, flags, raw elements).
- `nft::Packet` (fake IP/TCP/UDP packet built from the socket), `Iif`, `Outcome` (accept, drop,
  reject with errno, dnat), `Eval`; namespace-side packet construction is in `kernel/netns`.

## Public entry points

- `nft::handle` handles one netfilter datagram.
- `Nft::nat`, `Nft::filter`, `has_nat`, `has_filter` and `tracks` serve namespace and socket callers.

## Data flow / lifecycle

### nftables control plane

- One ruleset per namespace; every request (queries too) needs `CAP_NET_ADMIN` over the
  namespace owner. Changes come in `BATCH_BEGIN`/`END` pairs within one datagram: the batch runs
  on a clone, every error is reported, and the clone replaces the ruleset only if all succeeded;
  `NFNL_BATCH_GENID` mismatches `ERESTART`; the generation increases per committed batch.
- Tables (families inet, ip, ip6, arp, bridge, netdev; only `DORMANT` flag, `OWNER` `EOPNOTSUPP`),
  chains (base chains with hook, priority, type filter/nat/route, policy; existing chains can only
  change policy and userdata; rename is silently ignored; `EBUSY` when referenced), rules (raw
  expressions; position, append, replace — a replaced rule gets a new handle), sets and elements
  (key length 1..64, `%d` names, anonymous sets dropped when unused; `EBUSY` while referenced).
  Stateful objects and flowtables `EOPNOTSUPP`; `DESTROY*` maps to delete ignoring `ENOENT`.
  `DELTABLE`/`DESTROYTABLE` with family 0 or without a name and handle flush instead (Linux's
  `nft_flush`, what `nft flush ruleset` sends): every table of the family, of all families for 0,
  only those with the name when one is given; matching nothing succeeds. Other requests with
  family 0 are `EAFNOSUPPORT`. `DELTABLE` ignores `NLM_F_NONREC`; verdict-map references are not
  counted as chain uses.
- Validation: expression names must be known (39); jump targets must exist and not be base chains;
  loops `EMLINK`; set references rewritten to names. Arguments are not validated.
- Queries outside batches: `GETGEN`, dumps of tables, chains, rules (filters by table and chain),
  sets and elements in 6.8's format, packed into 8 KiB datagrams (a single larger message, and a
  set-element dump, which is one message, go out unbounded); `nft_compat` revision queries;
  empty ctnetlink dumps. No `NLM_F_ECHO`, no monitor notifications. Base-chain counters are 0.
- An initial-namespace commit re-evaluates port forwarding.

### Expression evaluation

- Expressions: `meta` (len, protocol, mark get/set, iif/oif and names, skuid/skgid on OUTPUT,
  nfproto, l4proto, pkttype), `payload`, `cmp`, `bitwise`, `lookup`, `immediate`, `ct` (state is
  NEW, or ESTABLISHED for UDP to a peer already heard from; others no match), `fib` (address type
  only), `nat` (DNAT, range minimum; SNAT no-op), `redir` (127.0.0.1/`::1` on OUTPUT, the input
  interface's address on PREROUTING), `reject`, `queue` (drop), `limit` (a token bucket per rule
  expression, packets or bytes, `over` inverting) and `quota` (bytes counted per rule expression,
  `over` inverting); a packet's length, for them as for `meta len`, is its headers only. `counter`,
  `log`, `notrack`, `dynset`, `objref`, `last`, `masq`, `connlimit`, `dup` always match without
  effect. xt targets DNAT, REDIRECT, MARK, ACCEPT, DROP, REJECT (others no-ops) and matches
  comment, mark, owner, conntrack, state, addrtype, tcp, udp, limit (the same bucket from avg and
  burst; others never match). Unknown or
  unevaluable expressions break the rule. Jumps to missing chains or deeper than 16 drop; 100,000
  steps accept; chains end in their policy.
- `kernel/netns` builds packets and selects hooks at connect/sendto and forwarding probes;
  matching-family base chains run in priority order. `Outcome` returns accept, drop, reject
  with errno or DNAT to that caller. `tracks()` decides whether `SO_ORIGINAL_DST` answers.
- POSTROUTING (nat and filter), INPUT-hook nat and arp/bridge/netdev tables are never evaluated
  by the integration; SNAT and MASQUERADE have no effect.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/netlink` dispatches `NETLINK_NETFILTER` requests through `KernelHost`.
- `kernel/netns` constructs packets and chooses hooks; `kernel/net` queries NAT/conntrack presence.

### Outbound

- `kernel/ns` owns `NetNs` and its `nft` field; `kernel/cred` checks namespace capability.
- `kernel/netlink` provides message/attribute encoding.
- `kernel/netlink` compares the initial ruleset generation before/after requests and calls
  `kernel/net` to refresh `kernel/netns` forwarding when it changes.

## Invariants and gotchas

- Rulesets are per network namespace; requests, including queries, require `CAP_NET_ADMIN`.
- Evaluation uses synthetic headers, not host packets. Hook placement and verdict application
  must stay consistent with `kernel/netns`; changing either side requires reading both maps.

## Confirmed bugs / technical debt

- **Per-key rate limits do nothing:** meters (`dynset` with a limit per key) always match, and the
  xt `hashlimit` and `recent` matches never match, so ufw's `limit` rules (built on `recent`) never
  trigger; byte limits and quotas count headers only, so they let through more than Linux.
- **nft control-plane gaps:** a replaced rule gets a new handle; chain rename is ignored;
  `DELTABLE` ignores `NLM_F_NONREC`; chains used only from verdict maps can be deleted (their jumps
  then drop); no `NLM_F_ECHO` and no monitor notifications; set-element dumps are one unbounded
  message.
- POSTROUTING, SNAT and MASQUERADE have no effect; unsupported expression effects are listed above.

## Open questions

None.

## To verify

- Linux 6.1's results for `NLM_F_NONREC` on `DELTABLE` and generation bumps for no-op batches (taken
  from knowledge, not a source checkout).
