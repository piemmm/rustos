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
(`[fe80::1]:80`) so the `:port` separator is unambiguous. Ports are always
numeric: TAIRiX has no service-name database, so `-n` is accepted for
familiarity but is always in force for them. Addresses are numeric too
unless `-r` asks for host names.

### Options

| Option | Meaning |
|---|---|
| `-t`, `--tcp` | show TCP sockets (with neither `-t` nor `-u`, both show) |
| `-u`, `--udp` | show UDP sockets |
| `-a`, `--all` | show both listening and connected sockets |
| `-l`, `--listening` | show only listening sockets |
| `-n`, `--numeric` | numeric *service* names (always in force) |
| `-r`, `--resolve` | resolve addresses to host names over DNS |
| `-p`, `--processes` | add the owning-process column (`pid=N`) |
| `-4`, `--ipv4` | restrict to IPv4 sockets |
| `-6`, `--ipv6` | restrict to IPv6 sockets |
| `-H`, `--no-header` | suppress the header line |
| `-?`, `--help` | show the tool's own short help |

`-n` and `-r` are independent, exactly as in iproute2: `-n` governs service
names and `-r` host names, so `ss -rn` resolves hosts and leaves ports
numeric. Under `-r` each endpoint is looked up through the shared stub
resolver (`lib/resolver`, a `PTR` query) over **one** transport for the whole
listing, memoised per distinct address — a negative answer included — so a
peer on many rows costs one query, and the unspecified address is never
queried because it names no host. An address with no record stays numeric,
and without `-r` no socket is opened and nothing goes on the wire. On a host
whose resolver is unreachable each distinct address costs that resolution's
retry budget once, which is why the switch is opt-in.

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
- `stats:net/<iface>/rx.filtered` — frames the device's receive pre-filter
  shed before the stack was woken for them (`plans/NETWORK.md` N17d). It is
  distinct from `rx.dropped`, which counts frames the stack *did* receive
  and then discarded, and it is what makes a filter's effect visible: on a
  busy segment it climbs steadily while `rx.packets` does not. A driver
  reports it cumulatively, because it drains its rings on its own device
  interrupt and the stack does not ask for every batch — and it rides the
  notify that wakes the stack, not only the `Service` reply, so a
  receive-only interface (which rings no doorbell at all) reports a live
  figure rather than whatever its last transmit happened to observe. A bond
  reports the sum over its members' devices, as its other counters already
  aggregate them. It advances in *steps* rather than smoothly: a harvest
  that admits nothing sends no notify — that suppression is the whole point
  — so the count catches up whenever anything else wakes the stack. It is
  cumulative, so none of it is lost.
- `stats:net/<iface>/rx.pps`, `.../rx.bps` (and `tx.*`) with a mandatory
  `?window=<duration>` decoration (`500ms`, `1s`, `2m`) — the average
  rate over the window that actually elapsed.
- `stats:net/stack/icmp-errors`, `.../icmp-suppressed`,
  `.../reassembly-evicted` — the packet-path defence counters, summed
  across interfaces, so a denial-of-service in progress is visible.
- `stats:net/stack/syn-cookies`, `.../syn-cookies-accepted`,
  `.../syn-cookies-rejected`, `.../syn-backlog-started`,
  `.../syn-backlog-expired`, `.../accepts`, `.../accept-overflow`,
  `.../tcp-resets` — the TCP connection-defence totals, so a SYN flood is
  visible as the cookie brake engaging. These come from the stack's one
  socket table rather than being summed per interface, and each is
  monotonic over the boot: a listener that closes folds its totals into the
  stack's, so a flood that ended with its target socket closing does not
  vanish from the count.

These resolve through `tairix_procinfo`'s `info:`/`stats:` resolver onto the
`NET_INTERFACE_COUNTERS`, `NET_INTERFACE_RATES`, and `NET_STACK_DEFENCE`
queries; like the socket table they require `CAP_SYSINFO_GLOBAL` and are
audited.

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

## Observing the resolver servers

The host's active recursive-resolver (DNS) servers are addressable the same
way — TAIRiX's equivalent of reading `/etc/resolv.conf`:

- `state:net/resolver/servers` — the comma-separated recursive DNS servers
  the host will query (V4 as dotted-quad, V6 in RFC 5952 form, in the
  stack's order), or `none` when it has learned none.

The set is each managed interface's statically configured servers
(`<iface>.dns.servers`) followed by its DHCP-learned servers (DHCPv4
option 6, DHCPv6 option 23), aggregated across every interface,
deduplicated, and bounded — static servers rank first as the operator's
explicit choice. Unlike the socket, counter, and bond reads above, this is **not**
privileged: the recursive-server list is public host configuration and
exposes no per-principal secret, so the `NET_RESOLVER_SERVERS` query is
ungated (no capability, no audit). It resolves through the same
`tairix_procinfo` resolver onto that query, which the `sysinfod` broker
forwards to `netstack`'s `NetstackRequest::ResolverServers` broker read.

## `host` — DNS lookups

`host` resolves a name over DNS from the shell, in the familiar
`bind-utils` shape:

```
$ host example.com
example.com has address 93.184.216.34
example.com has IPv6 address 2606:2800:220::1
```

An **address** operand is the reverse direction:

```
$ host 10.0.2.2
2.2.0.10.in-addr.arpa domain name pointer gateway.example.
```

With no `-t` a name looks up both the `A` (IPv4) and `AAAA` (IPv6) records
and an address looks up `PTR`; `-t A` / `-t AAAA` / `-t PTR`
(case-insensitive) restricts the lookup to one, applied to the operand's
reverse name when the operand is an address (the `bind-utils` behaviour).
Other record types (`MX`, `TXT`, …) are rejected rather than silently
treated as `A` — the stub resolver looks up address and pointer records only.
A name that does not exist prints `Host <name> not found: 3(NXDOMAIN)`; when
no server can be reached, the timeout is reported on standard error. `host`
exits `0` when at least one address was found, `1` when the name resolved to
no address, and `2` on a usage or output error.

`host` is a client of the shared userland stub resolver (`lib/resolver`): it
reads the active recursive-server set through the same ungated
`NET_RESOLVER_SERVERS` query the `state:net/resolver/servers` read exposes,
then drives the pure RFC 1035 / RFC 5452-hardened DNS engine in `lib/net`
over an ordinary `CAP_NET` UDP socket (with CSPRNG source-port
randomisation and a random query id). There is no `/etc/resolv.conf` and no
local host file.

## `ping` — reachability and round-trip time

`ping` sends ICMP/`ICMPv6` echo requests through the capability-gated echo
socket and prints each reply with its sequence number and time, then a
closing statistics block, in the familiar iputils shape.

```
$ ping -c 3 gateway.example
PING gateway.example (10.0.2.2) 56(84) bytes of data.
64 bytes from gateway.example (10.0.2.2): icmp_seq=1 time=0.412 ms
64 bytes from gateway.example (10.0.2.2): icmp_seq=2 time=0.388 ms
64 bytes from gateway.example (10.0.2.2): icmp_seq=3 time=0.401 ms

--- gateway.example ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 2003ms
rtt min/avg/max = 0.388/0.400/0.412 ms
```

The target is an address literal **or a host name**, resolved through the
same `lib/resolver` stub resolver `host` and `telnet` use — the
literal-first, family-preference policy is shared (`resolve_host`), so the
three cannot disagree about what a host operand means, and a literal needs
no query at all.

### The payload is high-entropy by default

Each request carries random bytes, drawn fresh for every request. This is a
deliberate divergence in *default*, not in option surface: a link that
compresses or de-duplicates traffic would otherwise report a throughput and
latency that say nothing about its real capacity, which defeats the
measurement. The echoed bytes are compared with what was sent, so a random
payload doubles as a per-packet integrity check.

`-p <hex>` opts into a fixed repeating pattern (iputils' spelling) where a
deterministic payload is wanted; `-p random` names the default explicitly.
The bytes come from `lib/rng`'s fast xoshiro256++ generator seeded once from
the kernel CSPRNG — bulk uncompressible data is not a security surface, so
drawing every payload from the CSPRNG would spend the reserve for nothing.

### Deliberate divergences

* No `ttl=` field on a reply line: the IP time-to-live is not exposed
  through the echo-socket interface.
* The peer is reverse-resolved once per run, so a reply line reads
  `<n> bytes from <name> (<address>)` when the address has a `PTR` record
  and carries the bare address when it does not. `-n` suppresses the lookup
  entirely: no `PTR` query goes on the wire and every line stays numeric.
  (Not a divergence — iputils behaves the same way; it is listed here
  because the header line keeps the operand as typed rather than the
  resolved name.) The lookup precedes the first request, so on a host whose
  resolver is unreachable it costs that resolution's retry budget before any
  packet leaves — `-n` is the switch for exactly that case.

## `telnet` — the Network Virtual Terminal client

`telnet` opens a TCP connection to a host and relays the terminal to it, in the
familiar BSD/inetutils shape:

```
$ telnet example.test
Trying example.test...
Connected to example.test.
Escape character is '^]'.
login:
```

The host may be a name or a literal IPv4/IPv6 address; a name is resolved
through the same shared `lib/resolver` policy `host` and `ping` use. The port is a
number — TAIRiX has no services database, so a service *name* is a usage error
rather than a silent fall back to port 23. It is equally the way to poke a TCP
service by hand: `telnet host 80` opens a connection you can type a request
into. With no host at all, `telnet` starts at its own `telnet>` prompt and
`open` connects.

`-4`/`-6` restrict the address family, `-8`/`-L` request an 8-bit data path,
`-e`/`-E` set or remove the escape character, `-a`/`-l` export a login name
over NEW-ENVIRON, `-b` binds a local address, and `-d` traces the negotiation
on standard error. The escape character (`^]` by default) drops into the
`telnet>` interpreter: `open`, `close`, `quit`, `logout`, `display`, `mode`,
`status`, `send`, `set`, `unset`, `toggle`, `environ`, `slc`, `z` and `?`, each
accepting the unambiguous prefixes BSD telnet accepts.

### Protocol scope

Negotiation follows RFC 855 with the RFC 1143 loop-free Q Method, so a peer
that repeats a request forever draws exactly one answer. The implemented
options are BINARY (RFC 856), ECHO (RFC 857), SUPPRESS GO AHEAD (RFC 858),
STATUS (RFC 859), TIMING MARK (RFC 860), TERMINAL TYPE (RFC 1091), NAWS
(RFC 1073), TERMINAL SPEED (RFC 1079), TOGGLE FLOW CONTROL (RFC 1080),
LINEMODE (RFC 1184) and NEW-ENVIRON (RFC 1572). **Every other option is
refused** — which is what an unimplemented option means; a client that
accepted one it could not honour would be lying to the server.

A subnegotiation is honoured only for an option that is actually enabled, as
RFC 855 requires: without that gate a server that never asked could make the
client disclose the operator's exported `NEW-ENVIRON` variables, its terminal
type and its window size purely on request.

RFC 1184 LINEMODE is implemented in full: the `MODE` mask with its
acknowledgement discipline, the Set Local Characters table with the RFC's
level/ack rules, and `FORWARDMASK`. The local line editor those characters
drive is assembled from `lib/vt`'s shared control vocabulary and Delete-key
recogniser, so `telnet` agrees with the console and the shell about which
keystroke rubs one character out.

Everything the remote host sends is attacker-controlled, so the receive parser
is total and bounded: an over-long or malformed subnegotiation is discarded
whole and parsing resumes at `IAC SE`, and nothing partial is ever applied.
The `fuzz_telnet` harness asserts, over arbitrary bytes, that the parser never
panics, that its held subnegotiation stays inside its fixed bound, that a live
session's reply is bounded by its input rather than amplifying it, and that
every byte the client emits re-parses as well-formed telnet.

### Deliberate divergences

`telnet` tracks the historical tool closely, and where it differs it says so:

| Difference | Why |
|---|---|
| No `!` shell escape | Giving a program that parses hostile network input the authority to spawn a shell inverts the minimum-capability posture the tool is built on. Use `z`, or another terminal. |
| No `slc check` | RFC 1184 gives it no wire form distinct from `slc export`. |
| No `-n tracefile`, `-r`, `-c` | There is no trace file, no rlogin, and no `/etc`. They are unknown options, not accepted-and-ignored ones. |
| A Synch is the bare Data Mark | The socket ABI exposes no TCP urgent data, so there is nothing in flight to discard ahead of it. |
| A resize reaches the host at the next keystroke | There is no window-change signal to park on, so the grid is re-read on each keyboard event. |
| A server-sent `AYT` is answered on screen | Injecting bytes into the server's own input stream is not a client's place. |
| End of standard input half-closes rather than exiting | The historical tool exits, which discards whatever the server was still sending — `telnet host 80 < request` loses the response. Closing only the write side is what a TCP client does, so the response arrives and the peer's own close ends the session. |

`NEW-ENVIRON` discloses **only** variables the operator defines and exports
with the `environ` command; the client never sends its own environment. `-a`
and `-l` export a login name, and that is the one thing an invocation discloses
by itself.

### Where the authority comes from

The session runs over an ordinary `SocketType::Stream` socket obtained through
the `netsock-v1` ABI under `CAP_NET`, audited and fail-closed; a host name is
resolved over an ordinary UDP socket by the same stub resolver `host` drives.
`CAP_CONSOLE_READ` covers the raw-mode keystrokes it relays (and authorises the
input-mode switch), `CAP_CONSOLE_WRITE` the output, and `CAP_FS_ACCESS` its own
`Help/` tree. Connecting *to* a well-known port is unprivileged — only binding
one needs `CAP_NET_BIND_PRIVILEGED` — so `telnet` requests neither that nor
`CAP_NET_RAW`. The full design is `plans/TELNET.md` in the source tree.

## Configuring interfaces (`network.conf`)

Per-interface addressing is declarative, not imperative: it lives in one
document, `/System/Settings/Network/network.conf`, parsed and rendered by
the single `lib/netconfig` engine. Each managed interface is named by a
stable admin alias (`wan`, `lan0`) bound to hardware by its MAC
(`<iface>.match.mac`) — never by discovery order — and carries its
addressing (`<iface>.ipv4.method static|dhcp|disabled` — `static` with
`ipv4.address`/`ipv4.gateway`, `dhcp` leasing them over RFC 2131 DHCPv4;
`<iface>.ipv6.method slaac|static|dhcp|disabled` — `static` with
`ipv6.address`/`ipv6.gateway`, `dhcp` leasing an address over RFC 8415
stateful DHCPv6), an optional comma-separated `<iface>.dns.servers` list of
recursive DNS servers (which join the interface's DHCP-learned servers in the
active resolver set), and an optional `<iface>.mtu`.

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
`net.tcp.keepalive`, `net.tcp.ecn`) live separately in `system.conf` and
are set with `configure` (§6.2). See
[Network-stack service](./netstack.md) for how the configuration is
delivered and enforced.

## See also

- [Network-stack service](./netstack.md) — the `netstack` process, its
  request surface, and the interface admin API.
- [Network security posture](../security/network.md) — the threat model,
  defences, and the network audit event-id registry.
- [System Information service](./sysinfod.md) — the broker every query
  passes through.
