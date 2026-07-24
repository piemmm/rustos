# tairix-ping

TAIRiX `ping` — send ICMP/`ICMPv6` echo requests to a network host, in the
familiar iputils shape (`plans/NETWORK.md` N8b-2b, a `plans/APPS.md`
command app).

`ping` sends echo requests to a host and prints each reply with its
sequence number and round-trip time, then a closing statistics block.
`-c` bounds the request count, `-i` sets the interval, `-s` the payload
size, `-W` the per-reply timeout, `-w` an overall deadline, `-4`/`-6`
force the address family, `-q` is quiet, and `-n` is accepted but always
in force (there is no name resolver in this release, so the target must be
a literal IPv4 or IPv6 address). `-?`/`--help` render the tool's own short
help.

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
* `net` — the `PingIo` seam (clock, echo socket, wait/park) and the
  owned `EchoReply`.
* `client` — the `run` entry point, the ping loop, and the statistics.
* `run.rs` — the freestanding `Run` binary (pure-Rust, `tairix-rt`), which
  implements `PingIo` over the `tairix_rt::net` echo-socket wrappers.

The bundle's `Help/` documents are authored on disk and read at runtime
through the injected `tairix_help::HelpSource`; help is never embedded in
the program (`plans/APPS.md`).

## Stability

Experimental. The ICMP-echo socket path tracks the unfrozen `abi-v1`
`netsock-v1` contract and may evolve until the first release.
