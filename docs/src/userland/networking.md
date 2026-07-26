# Networking tools

TAIRiX exposes its live network state to the terminal the same way it
exposes everything else: through the typed, versioned, capability-checked
System Information API (§16.6), never a `/proc/net`-style pseudo-filesystem
and never free-form text scraping. This page documents the user-facing
tools built on that surface. Their per-command option and output
specifications are binding in `plans/APPS.md` and follow the established
iproute2 / net-tools conventions (§16.7); this page is the orientation.

## `ss` — socket statistics

`ss` lists the system's open sockets, one row per socket, in the familiar
iproute2 shape:

```
Netid State  Recv-Q Send-Q Local Address:Port   Peer Address:Port
tcp   ESTAB  0      0      10.0.2.15:4321       10.0.2.2:80
tcp   LISTEN 0      0      *:777                *:*
udp   UNCONN 0      0      *:5353               *:*
```

Each row carries the transport protocol (`tcp`/`udp`), the connection
state, the receive and send queue depths, the local and peer
`address:port`, and — with `-p` — the owning process. An unspecified
address or port prints as `*`; an IPv6 address is bracketed
(`[fe80::1]:80`) so the `:port` separator is unambiguous. Ports and
addresses are always numeric: TAIRiX has no service-name database, so
`-n` is accepted for familiarity but is always in force.

### Options

| Option | Meaning |
|---|---|
| `-t`, `--tcp` | show TCP sockets (with neither `-t` nor `-u`, both show) |
| `-u`, `--udp` | show UDP sockets |
| `-a`, `--all` | show both listening and connected sockets |
| `-l`, `--listening` | show only listening sockets |
| `-n`, `--numeric` | numeric ports/addresses (always in force) |
| `-p`, `--processes` | add the owning-process column (`pid=N`) |
| `-4`, `--ipv4` | restrict to IPv4 sockets |
| `-6`, `--ipv6` | restrict to IPv6 sockets |
| `-H`, `--no-header` | suppress the header line |
| `-?`, `--help` | show the tool's own short help |

By default `ss` shows connected, non-listening sockets, matching the
iproute2 default; the count of hidden listeners is noted on the standard
information stream (fd 3) with the `net.listening_omitted` record (§20.1),
never in the table. `ss` accepts options only — the iproute2
filter-expression grammar (state and address filters) is not implemented,
so a bare operand is a usage error rather than a silently ignored
argument.

### Where the rows come from

`ss` is a *selection and rendering engine*, not a data source. It reads
the socket table through the shared `tairix_procinfo::for_each_net_socket`
paging walk over the `sysinfo-v1` `NET_SOCKETS` query, which the
`sysinfod` broker forwards to `netstack`'s capability-gated broker read
(the `NetstackRequest::Sockets` operation). There is no second query
client and no `/proc/net`.

The listing names every principal's sockets and every connection's peer
address, so it is a **privileged, system-wide diagnostic**: `NET_SOCKETS`
requires `CAP_SYSINFO_GLOBAL` and every query is audited. A session
without that capability is told so on standard error and `ss` exits
non-zero, rather than printing an empty table a reader would mistake for
"no sockets" (fail loud, §24). The per-socket record is
`tairix_abi::net_ipc::NetSocketRecord`.

## Observing interface counters and rates

Beyond the socket table, every managed interface's counters and windowed
throughput rates are addressable as resource references and read with the
same System Information API (`plans/ALIAS.md` §6, `plans/NETWORK.md` §5):

- `stats:net/<iface>/rx.packets`, `.../rx.bytes`, `.../rx.dropped` and the
  `tx.*` counterparts — the monotonic per-interface counters.
- `stats:net/<iface>/rx.pps`, `.../rx.bps` (and `tx.*`) with a mandatory
  `?window=<duration>` decoration (`500ms`, `1s`, `2m`) — the average
  rate over the window that actually elapsed.
- `stats:net/stack/icmp-errors`, `.../icmp-suppressed`,
  `.../reassembly-evicted` — the stack-wide defence counters, summed
  across interfaces, so a denial-of-service in progress is visible.

These resolve through `tairix_procinfo`'s `info:`/`stats:` resolver onto
the `NET_INTERFACE_COUNTERS` and `NET_INTERFACE_RATES` queries; like the
socket table they require `CAP_SYSINFO_GLOBAL` and are audited.

## Observing bonds (link aggregation)

A bond interface's members and their live failover state are addressable
the same way (`plans/NETWORK.md` §5, §6.3):

- `info:net/<bond>/members` — the member interface aliases, in configured
  order.
- `state:net/<bond>/active-member` — the currently-transmitting member in
  active-backup mode (`none` in balance mode or while the bond is down).
- `state:net/<bond>/member-health` — each member rendered as
  `member=up,eligible[,active]` (a failed member renders `member=down`),
  so a silently-failed link is a visible fact.

These resolve through the same resolver onto the `NET_BOND_MEMBERS` query,
which the `sysinfod` broker forwards to `netstack`'s capability-gated
broker read (the `NetstackRequest::BondMembers` operation); they require
`CAP_SYSINFO_GLOBAL` and are audited, and a non-bond alias fails closed.

## Configuring interfaces (`network.conf`)

Per-interface addressing is declarative, not imperative: it lives in one
document, `/System/Settings/Network/network.conf`, parsed and rendered by
the single `lib/netconfig` engine. Each managed interface is named by a
stable admin alias (`wan`, `lan0`) bound to hardware by its MAC
(`<iface>.match.mac`) — never by discovery order — and carries its
addressing (`<iface>.ipv4.method static|disabled` with
`ipv4.address`/`ipv4.gateway`; `<iface>.ipv6.method slaac|static|disabled`
with `ipv6.address`/`ipv6.gateway`) and an optional `<iface>.mtu`.

```
wan.match.mac    52:54:00:12:34:56
wan.ipv4.method  static
wan.ipv4.address 10.0.2.15/24
wan.ipv4.gateway 10.0.2.2
wan.ipv6.method  slaac
```

The file is read post-unlock by the device manager and applied to
`netstack` per interface, atomically and idempotently — a malformed or
inconsistent document is refused whole and the running configuration is
left untouched (fail closed). The image ships an empty document ("no
managed interfaces beyond loopback"); the installer, or a future
`configure`-class writer, fills in the operator's interfaces through the
same engine. The stack-wide switches (`net.ipv4.enabled`,
`net.ipv6.enabled`, `net.ipv6.privacy`, `net.tcp.syncookies`,
`net.tcp.keepalive`) live separately in `system.conf` and are set with
`configure` (§6.2). See
[Network-stack service](./netstack.md) for how the configuration is
delivered and enforced.

## See also

- [Network-stack service](./netstack.md) — the `netstack` process, its
  request surface, and the interface admin API.
- [Network security posture](../security/network.md) — the threat model,
  defences, and the network audit event-id registry.
- [System Information service](./sysinfod.md) — the broker every query
  passes through.
