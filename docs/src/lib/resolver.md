# `tairix-resolver`

The userland DNS stub-resolver client (`plans/DNS.md` DNS2). Stability tier:
**experimental**.

`tairix-resolver` is the thin seam that turns the pure DNS engine in
[`tairix-net`](./net.md) (`tairix_net::dns`) into a working name lookup for a
userland program. It owns no DNS logic of its own: the RFC 1035 wire codec,
the RFC 5452-hardened response validation, and the retransmit/failover state
machine all live in `tairix-net`, and the active recursive-server set comes
from the one System Information API query (`NET_RESOLVER_SERVERS`) that the
`state:net/resolver/servers` read also uses — so a resolver client and an
operator inspecting the configuration can never disagree (`AGENTS.md` §2.2).

## Surface

- `route_name(name)` / `route_address(address)` — the one policy point that
  decides where a lookup is answered. A name under `local` (RFC 6762 §3) and a
  link-local address (`169.254.0.0/16`, `fe80::/10`, RFC 6762 §4) are the
  link's, answered by link-local discovery through `lib/discovery`; every
  other name and address is the configured servers'. A link name is never sent
  to a unicast server, so it cannot leak off the link.
- `resolve_name(name, record_type, sysinfo, udp, link, rng)` — the pure,
  host-testable orchestration. It routes the name; a server name fetches the
  configured recursive servers through an injected `tairix_procinfo::Transport`
  (the `NET_RESOLVER_SERVERS` paging walk) and drives `tairix_net::dns::resolve`
  over an injected `tairix_net::dns::DnsTransport` and CSPRNG, and a link name
  is answered by the injected `LinkLookup`. Every seam is injected, so the whole
  path is exercised against in-memory fakes with no kernel. A build without
  discovery answers a link name with nothing — `NonExistent`, not an error —
  exactly as a link with no such host does.
- `resolve_pointer(address, sysinfo, udp, link, rng)` — the reverse direction: the
  `PTR` lookup of the `in-addr.arpa` / `ip6.arpa` name an address maps back
  to, over the same servers, the same engine, and the same orchestration, or
  over the link for a link-local address.
  `pointer_name(&resolution)` renders what a tool prints, or `None` when the
  address has no record.
- `configured_servers(sysinfo)` — the server-set fetch on its own, converting
  each `NetResolverServer` record to an `IpAddr`.
- `ResolveError` — the distinct, actionable failure causes: `InvalidName`,
  `NoServers`, `ServerSource`, and `Transport`. A negative or timed-out
  lookup is **not** an error — it is returned as the corresponding
  `Resolution`.
- `RtLinkLookup` (the `program` feature) — the production `LinkLookup`: one
  `lib/discovery` session per lookup, bounded by a three-second window, since a
  responder on the link answers within a second or not at all.
- `RtDnsTransport` and `resolve(name, record_type)` (the `program` feature) —
  the production glue: a `DnsTransport` over the `netsock-v1` UDP datagram
  socket (`tairix_rt::net`). Opening one keys its query-id generator once from
  the kernel CSPRNG — a transport that cannot be keyed is refused rather than
  run on predictable ids — and binds a process-private delivery port
  (`tairix_rt::bind_private_port`). It opens the
  datagram socket for a server's address family on demand with a CSPRNG-drawn
  ephemeral source port (the RFC 5452 source-port randomisation the socket
  layer contributes), and parks on the delivery port for the reply — never a
  busy spin (`AGENTS.md` §2.23). `RtDnsTransport::resolve` reuses one bound
  port across several record-type lookups (the `host` A+AAAA default), so the
  port is bound once.
- `RtDnsTransport::reverse_name(address)` and the one-shot free
  `reverse_name(address)` (the `program` feature) — the reverse lookup over
  the production seams. A tool resolving many addresses opens one transport
  and calls the method, so the delivery port is bound once for a whole
  listing; a tool with a single lookup calls the free function.

## Security

The resolver adds no authority: opening the UDP socket is capability-gated
stack-side (`CAP_NET`), the server-set query is ungated public host
configuration (the resolv.conf analogue), and every response is validated by
the pure engine before an address is surfaced. Every DNS server and packet on
the wire is treated as hostile (`AGENTS.md` §26.4): off-path spoofing is
bounded by the engine's random query id and strict question match, and by
this crate's source-port randomisation and the socket layer's check of every
received datagram's kernel-attested sender against the network stack's service
account (a datagram from any other sender is discarded unread — fail closed).

## Consumers

The `host` command app is the first consumer; a future `nslookup`-class tool
would reuse the same client. There is no second DNS implementation or query
client.
