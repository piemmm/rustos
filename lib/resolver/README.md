# tairix-resolver

TAIRiX userland DNS stub-resolver client (`plans/DNS.md` DNS2). Stability
tier: **experimental**.

This crate is the thin seam that turns the pure DNS engine in `tairix-net`
(`tairix_net::dns`) into a working name lookup for a userland program. It
owns no DNS logic: the RFC 1035 wire codec, the RFC 5452-hardened response
validation, and the retransmit/failover state machine all live in
`tairix-net`, and the active recursive-server set comes from the one System
Information API query (`NET_RESOLVER_SERVERS`) that the
`state:net/resolver/servers` read also uses, so a resolver client and an
operator inspecting the configuration can never disagree (`AGENTS.md` §2.2).

## What it provides

- `resolve_name(name, record_type, sysinfo, udp, rng)` — the pure,
  host-testable orchestration: fetch the configured recursive servers through
  an injected `tairix_procinfo::Transport`, then drive
  `tairix_net::dns::resolve` over an injected `tairix_net::dns::DnsTransport`
  and CSPRNG. Both seams are injected, so the whole path is exercised against
  in-memory fakes with no kernel.
- `configured_servers(sysinfo)` — the server-set fetch on its own, converting
  each `NetResolverServer` record to an `IpAddr`.
- `RtDnsTransport` and `resolve(name, record_type)` (the `program` feature) —
  the production glue: a `DnsTransport` over the `netsock-v1` UDP datagram
  socket (`tairix_rt::net`), with RFC 5452 source-port randomisation from the
  port-0 bind and a kernel-attested stack-origin check on every received
  datagram (fail closed), plus a convenience entry point wired to the real
  sysinfo transport and the kernel CSPRNG.

## Security

The resolver adds no authority: opening the UDP socket is capability-gated
stack-side (`CAP_NET`), the server-set query is ungated public host
configuration, and every response is validated by the pure engine before an
address is surfaced. Every DNS server and packet is treated as hostile
(`AGENTS.md` §26.4): off-path spoofing is bounded by the engine's random query
id and strict question match, and by this crate's source-port randomisation
and origin check.

## Layering & safety

`no_std` (with `alloc`); as a `lib/*` crate it depends only on other `lib/*`
crates (`AGENTS.md` §17.4): `tairix-abi`, `tairix-net`, `tairix-procinfo`, and
— under the `program` feature only — `tairix-rt`. `#![forbid(unsafe_code)]`,
and no `unwrap`/`expect`/`panic!` on a production path.

## The shared host-operand policy

`resolve_host` is the one definition of what a command-line host operand
means: an address literal resolves with no query at all (so a literal target
works on a machine with no resolver configured), otherwise the wanted record
types are tried in family-preference order — IPv6 then IPv4 unless `-4`/`-6`
forced one. `ping`, `telnet`, and any future connecting tool call it, so they
cannot disagree about `-4 ::1`. `address_parts` is the matching split into
the `(family, 16-byte address)` pair every `netsock-v1` address field
carries.

The policy is pure and injected (`query` is a closure), so it is host-tested
against scripted resolutions; `rt::host_address` supplies the production
query and answers a literal without opening a socket.
