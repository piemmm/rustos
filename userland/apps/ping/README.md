# tairix-ping

TAIRiX `ping` — send ICMP/`ICMPv6` echo requests to a network host, in the
familiar iputils shape (`plans/NETWORK.md` N8b-2b, a `plans/APPS.md`
command app).

`ping` sends echo requests to a host and prints each reply with its
sequence number and round-trip time, then a closing statistics block.
`-c` bounds the request count, `-i` sets the interval, `-s` the payload
size, `-W` the per-reply timeout, `-w` an overall deadline, `-p` a fixed
payload pattern, `-4`/`-6` force the address family, `-q` is quiet, and
`-n` is accepted and inert (no reverse lookup is ever performed, so the
addresses printed are already numeric). `-?`/`--help` render the tool's
own short help.

The target operand is an IPv4 or IPv6 address literal **or a host name**.

## Payload

Each request carries **high-entropy random data by default, drawn fresh
per request**. A link that compresses or de-duplicates traffic would
otherwise report a throughput and latency that say nothing about its real
capacity — which defeats the measurement the tool exists to make — and
the echoed random bytes double as a per-packet integrity check. `-p <hex>`
opts into a deterministic repeating pattern where one is wanted (a
pattern-sensitive fault, a codec under test); `-p random` names the
default explicitly.

The bytes come from `lib/rng`'s fast xoshiro256++ generator seeded once
from the kernel CSPRNG. Bulk uncompressible data is not a security
surface, so drawing every payload from the CSPRNG would spend the reserve
for nothing; a private generator is forbidden, so `lib/rng` owns both.

## Name resolution

A host name is resolved through the shared userland stub resolver
(`lib/resolver`), which reads the host's configured recursive servers from
the System Information API and drives the pure `lib/net` DNS engine. The
literal-first, family-preference policy (`resolve_host`) is shared with
`telnet`, so the two cannot disagree about what a host operand means, and
an address literal resolves with no query at all — so a literal target
still works on a machine with no resolver configured.

Reverse (`PTR`) lookup is not available: the stub resolver has no `PTR`
record type, so reply addresses are always printed numerically and `-n`
has nothing to suppress.

## How it reaches the network

The tool opens an ICMP/`ICMPv6` echo socket through the versioned
`netsock-v1` socket ABI served by `userland/net/netstack`, gated on
`CAP_NET` + `CAP_NET_RAW` and audited. The stack owns the echo identifier,
so a socket only ever receives replies to its own requests; the tool never
crafts a raw IP packet, never touches a device, and holds no ambient
authority (`AGENTS.md` §5.4). The IP time-to-live is not exposed through
the echo-socket interface, so — unlike some `ping` implementations — a
reply line carries no `ttl=` field.

## Structure

* `command` — the option grammar (`Command`/`Config`) and its parser.
* `error` — `PingError`, the fatal outcomes of `run`.
* `io` — the `Output` seam.
* `net` — the `PingIo` seam (name resolution, clock, echo socket, payload
  entropy, wait/park), `ResolveFailure`, and the owned `EchoReply`.
* `client` — the `run` entry point, the ping loop, and the statistics.
* `run.rs` — the freestanding `Run` binary (pure-Rust, `tairix-rt`), which
  implements `PingIo` over `lib/resolver`, the `tairix_rt::net` echo-socket
  wrappers, and `lib/rng`'s payload generator.

The bundle's `Help/` documents are authored on disk and read at runtime
through the injected `tairix_help::HelpSource`; help is never embedded in
the program (`plans/APPS.md`).

## Stability

Experimental. The ICMP-echo socket path tracks the unfrozen `abi-v1`
`netsock-v1` contract and may evolve until the first release.
