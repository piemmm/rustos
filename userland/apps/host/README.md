# host

TAIRiX `host` command app (`plans/DNS.md` DNS3) — look a name up over DNS.

`host` resolves a domain name to its addresses in the familiar `bind-utils`
shape and is the first consumer of the shared userland stub resolver,
`lib/resolver`.

## Supported behaviour

- `host <name>` — look up the `A` and `AAAA` records the stub resolver
  supports and print each answer:
  - `<name> has address <ipv4>`
  - `<name> has IPv6 address <ipv6>`
  - `<name> has no A record` / `<name> has no AAAA record` for an empty answer.
- `host -t <type> <name>` — restrict the lookup to one record type (`A` or
  `AAAA`, case-insensitive). Any other type (`MX`, `TXT`, …) is rejected
  honestly — the stub resolver looks up only address records in this stage.
- A name that does not exist prints `Host <name> not found: 3(NXDOMAIN)`; an
  unreachable server prints `;; connection timed out; no servers could be
  reached` on standard error.
- `-?`/`-h`/`--help` — the tool's own short help from its bundled `Help/`
  tree.

## Exit status

- `0` — at least one address was found (or the short help was written).
- `1` — the name resolved to no address (a negative answer, a timeout, or a
  resolver failure such as no configured server).
- `2` — a usage error or an output failure.

## How it resolves

Resolution is done by `lib/resolver`, which reads the host's configured
recursive DNS servers through the ungated `NET_RESOLVER_SERVERS` System
Information query (the one active-server set `state:net/resolver/servers` also
exposes) and drives the pure, RFC 1035 / RFC 5452-hardened DNS engine in
`lib/net` over an ordinary capability-gated UDP socket. There is no second DNS
implementation and no `/etc/resolv.conf`.

## Required capabilities

`CAP_CONSOLE_WRITE` (the inherited standard output/error streams),
`CAP_FS_ACCESS` (the tool's own `Help/` documents), and `CAP_NET` (the UDP
socket the resolver queries with — the stack re-checks it and audits the
open, fail-closed). The active-server query is ungated, so no `CAP_SYSINFO_*`
is requested. The tool reads no standard input.
