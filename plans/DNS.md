# DNS.md — Name resolution: the stub resolver (RFC 1035 / RFC 5452)

Staged build plan for TAIRiX's DNS name resolution. **Binding under
`AGENTS.md`** (read it first, especially §2, §4, §5, §17, §19, §24, §26); it
consumes the seams `plans/NETWORK.md` and `plans/DHCP.md` establish and never
contradicts them — where they touch, NETWORK.md's decisions stand. `abi-v1`
is not frozen (PLAN.md Stage 1): the ABI/config additions here are ordinary
pre-release changes (§2.13).

`plans/NETWORK.md` §9 defers DNS to "its own plan" as a *consumer* of the
socket ABI; this is that plan. DHCPv4 (RFC 2132 option 6) and DHCPv6 (RFC
3646 option 23) already learn the recursive name servers an interface should
use (`plans/DHCP.md`), so the resolver has a real input the moment it lands.

## 0. Scope and decisions (binding)

- **The resolver is a client of the recursive DNS servers the host is
  configured with; TAIRiX does not implement a recursive/authoritative
  server here.** It sends a standard query (RD=1) to a configured recursive
  resolver and interprets the answer — the classic "stub resolver" role
  (RFC 1034 §5.3.1). Iterative resolution, an authoritative server, a cache
  daemon, DNSSEC validation, and DNS-over-TLS/HTTPS are explicitly **not** in
  this plan (§2.3/§2.4); each is a later increment or its own plan.
- **One pure engine, host-testable: `lib/net::dns`.** All wire parsing,
  query building, and the resolver retry/failover state machine live in the
  pure, `no_std`, `#![forbid(unsafe_code)]`, allocation-bounded `lib/net`
  crate, driven by injected monotonic time and caller-supplied CSPRNG values
  (the query id and retransmission jitter) — the engine never generates
  randomness itself (the `dhcp` / `tcp::conn` `iss` precedent). This is the
  §2.2 one-definition rule: the unit tests, the fuzz harness, and any live
  consumer all exercise the *same* engine.
- **Every server and every packet on the wire is hostile (§26.4).** A DNS
  response is attacker-controlled and off-path spoofing is the canonical
  threat, so the codec is total, bounded (§24.4 fixed validation bounds — a
  255-octet name ceiling, a 63-octet label ceiling, a fixed-capacity answer
  list, a bounded compression-pointer follow count that cannot loop), fuzzed
  (§19.6), and fails closed: a malformed or inconsistent response is dropped
  whole; nothing partial is surfaced. Off-path spoofing is bounded by the
  RFC 5452 §9 defences the engine enforces — a CSPRNG-random 16-bit query id
  and a strict match of the response's question section (QNAME
  case-insensitively, QTYPE, QCLASS) against the outstanding query; a
  mismatch is discarded, never accepted. (Source-port randomisation, the
  other RFC 5452 defence, is the socket layer's job in DNS2, not the
  engine's.)
- **Event-driven, tickless (§2.23).** The resolver exposes a folded one-shot
  `next_deadline()`; the caller arms a single timer and calls `poll(now)`
  when it fires and `on_response(now, bytes)` when a datagram arrives.
  Retransmission uses randomised exponential backoff with a fixed retry
  budget per server and deterministic failover to the next configured server;
  when the budget across all servers is spent the query terminates as a
  timeout. There is no polling loop.
- **UDP, classic 512-byte messages, no EDNS0 yet.** DNS1 speaks plain RFC
  1035 UDP (no OPT record); a response with the TC (truncation) bit set is a
  soft failure that fails the current server (a future EDNS0 / TCP-retry
  increment removes that limit in place, §2.13). A CNAME chain in the answer
  is followed within the single response (RFC 1034 §3.6.2); the resolver does
  not itself re-query for an unresolved CNAME target in DNS1 (a recursive
  server returns the chain), and surfaces that case as `NoData`.

## 1. Target architecture (binding)

- `lib/net/src/dns.rs` — the pure engine:
  - **Wire vocabulary** — `Name` (a canonical, lowercased, bounded wire
    encoding of a domain name; `Name::encode` parses a dotted string with
    the LDH/label/length rules and `read_name` decompresses RFC 1035 §4.1.4
    pointers with a bounded follow count so a pointer loop cannot hang),
    `RecordType` (`A` / `AAAA`, the query types the stub resolves, plus the
    `CNAME` it must chase), `Rcode` (RFC 1035 §4.1.1 response codes), and the
    12-byte header field pack/unpack.
  - **Codec** — `write_query(&QuerySpec, buf)` emits one standard query
    (header with RD, one question); `DnsResponse::parse(bytes, &QuerySpec)`
    validates the header + echoed question against the outstanding query and
    surfaces the `Rcode`, the resolved address list (following any CNAME
    chain to the queried type), and the minimum answer TTL. Every decode is
    total/bounded/fail-closed.
  - **Resolver state machine** (`DnsResolver`) — construct with the name, the
    record type, and the bounded configured server list; `poll(now, rng)`
    starts/advances the query (picking a server, drawing a fresh random id,
    arming the retransmit deadline) and `on_response(now, bytes)` folds a
    datagram. Both return the `Action` the caller performs (`Send { query,
    server }` to transmit, or `Finished(Resolution)` when the answer,
    negative answer, or timeout is reached). `next_deadline()` is the folded
    one-shot. It never allocates and never blocks.
- `lib/net/tests/fuzz_net_dns.rs` — the `fuzz_net_dns` harness (registered in
  `tools/xtask`): arbitrary bytes through `DnsResponse::parse` (bounded,
  non-panicking), `write_query` (non-panicking, fixed-length), and the
  `DnsResolver` driven with crafted responses at arbitrary times.

## 2. Capabilities

DNS1 adds no capability and no ABI surface: the engine is pure `lib/net`. The
DNS2 socket path reuses the existing capability-gated UDP socket surface
(`netsock-v1`, `CAP_NET_*`) — a resolver is an ordinary unprivileged UDP
client to port 53; no new capability is warranted (§5.2 minimalism).

## 3. Staged increments

### DNS1 — the pure `lib/net::dns` engine `[x]`

The RFC 1035 codec + RFC 5452-hardened stub-resolver state machine as above,
with host unit tests (name encode/decode incl. compression + loop guard,
query emit, response parse incl. id/question mismatch rejection, CNAME chase,
NXDOMAIN/NODATA/ServFail handling, TC soft-fail, the retransmit/backoff/
failover/timeout lifecycle) and the `fuzz_net_dns` harness. Pure, no netstack
change — the engine stands alone, exactly as DHCP D1 did.

### DNS2 — the resolver over the socket ABI `[~]`

A small userland resolver seam that opens an unprivileged UDP socket to a
configured server on port 53, drives the `DnsResolver` engine over it
(source-port randomisation is the socket layer's RFC 5452 contribution), and
surfaces resolved addresses to a consumer. The configured server list is
sourced from the DHCP-learned servers and/or a `network.conf` key; a
`state:net/resolver/servers` read makes the active set observable (§5). Its
own live QEMU vertical (a host DNS peer answering a query) across the Tier-1
arches, mirroring the DHCP D3 pattern.

Done so far — the active-server aggregation and its observability surface:

- `Stack::dhcp_dns_servers()` surfaces an interface's DHCP-learned servers
  (IPv4 option 6, then IPv6 option 23) from the *live* lease.
- `Netstack::resolver_servers()` aggregates those across every managed
  interface, deduplicated and bounded by
  `tairix_abi::net_ipc::MAX_RESOLVER_SERVERS` (4), and serves them as the
  `NetstackRequest::ResolverServers` broker read (gated `CAP_SYSINFO_INTROSPECT`
  like the other `netstack` broker reads), framed as a shared page of
  `NetResolverServer` records (no bespoke reply codec).
- The System Information API exposes the same set as the **ungated**
  `SysinfoQueryId::NET_RESOLVER_SERVERS` query (the recursive-server list is
  public host configuration, the resolv.conf analogue), served by `sysinfod`
  (forwarding to the `netstack` broker) and read by `lib/procinfo`
  (`for_each_resolver_server` + the `state:net/resolver/servers` resolver
  branch, rendering the comma-separated set or `none`).
- **Static config key** — the per-interface `network.conf`
  `<iface>.dns.servers` key: a comma-separated unicast IPv4/IPv6 list in
  `lib/netconfig` (`IfaceKey::DnsServers`, bounded by `MAX_DNS_SERVERS` =
  `MAX_RESOLVER_SERVERS`, unicast-only, no member key), carried on the wire in
  `NetInterfaceConfigMsg` as `NetDnsServers` (a fixed count + four
  `NetResolverServer` slots, fail-closed decode), populated by `devmgr`
  (`dns_of`/`dns_record_of`) and stored per interface in `netstack`.
  `Netstack::resolver_servers()` unions each interface's static list **first**
  (the operator's explicit choice) then its DHCP-learned servers, so the same
  broker read now surfaces both sources.

Remaining, in order:

1. **`lib/resolver` client crate** — a freestanding+host-stub crate driving
   `DnsTransport` over `tairix_rt::net` (a datagram socket, port-0 bind for
   RFC 5452 source-port randomisation, `waitset_wait` delivery — the `ping`
   `RtPingIo` pattern) whose `resolve()` first fetches the servers (via the
   ungated `NET_RESOLVER_SERVERS` query) then drives `tairix_net::dns::resolve`.
   Landed **with** its first consumer (DNS3), never consumer-less (§2.4).
2. **3-arch live QEMU verticals** — `netstack_dns_qemu_{aarch64,x86_64,riscv64}`
   mirroring DHCP D3: a `NetPeerMode::V*DnsEcho` host DNS server in
   `tools/xtask` `netpeer`, a planted `network.conf` naming the server, a tiny
   guest resolver client, and witnesses.

### DNS3 — a `host`/`nslookup`-class command app `[ ]`

A system command app that resolves a name from the shell over DNS2,
GNU/`bind-utils`-compatible in options and output where it maps (§16.7), with
its `Help/` bundle and docs.

## 4. Tests, docs, and gate (binding)

Every increment lands its unit/property/fuzz/QEMU tests in the same change
(§7); every fuzz harness registers with `cargo xtask fuzz`; adversarial
corpora enter the regression corpus (§19.6). Every increment updates its
rustdoc + `docs/src/` pages and this plan's status marks in the same change
(§13) — status only, no landing narrative. Every increment ends with the full
§2.15 gate: `cargo fmt --all`, `cargo xtask ci` (once), `cargo xtask fuzz
--secs 5`, and `tools/ci/soak.sh both --secs 20`, quoted in the completion
report.

## 5. What this plan explicitly does *not* do

- No recursive or authoritative DNS server, no zone data, no caching daemon.
- No DNSSEC validation, no DNS-over-TLS / DNS-over-HTTPS, no EDNS0 (a later
  in-place extension, not speculated here — §2.4).
- No mDNS / LLMNR / NetBIOS name resolution.
- No `/etc/hosts`-style static host file (TAIRiX has no `/etc`, §16.1); a
  local static-name store, if ever wanted, is its own decision.
