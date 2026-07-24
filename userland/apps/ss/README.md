# tairix-ss

TAIRiX `ss` — list open sockets, in the familiar iproute2 shape
(`plans/NETWORK.md` N8b-2, a `plans/APPS.md` command app).

`ss` prints one row per open socket: the transport protocol, connection
state, receive/send queue depths, the local and peer `address:port`, and
— with `-p` — the owning process. The default view shows connected,
non-listening sockets; `-l` shows only listeners and `-a` shows both.
`-t`/`-u` filter by protocol, `-4`/`-6` by address family, `-H` drops the
header, and `-n` is accepted but always in force (TAIRiX has no
service-name database). `-?`/`--help` render the tool's own short help.

## Where the rows come from

Live state is read exclusively through the System Information API
(`AGENTS.md` §16.6): the versioned `sysinfo-v1` `NET_SOCKETS` query served
by `sysinfod`, which forwards to `netstack`'s capability-gated broker
read. There is no `/proc/net` and no second query client — the paging
walk is the shared `tairix_procinfo::for_each_net_socket`. The query names
every principal's sockets and every connection's peer, so it requires
`CAP_SYSINFO_GLOBAL` and is audited; a session without it is told so and
the tool exits rather than printing an empty table.

## Structure

* `command` — the option grammar (`Command`/`Options`) and its parser.
* `error` — `SsError`, the outcomes of `run`.
* `io` — the `Output` seam.
* `client` — the `run` entry point, filtering, and rendering.
* `run.rs` — the freestanding `Run` binary (pure-Rust, `tairix-rt`).

The bundle's `Help/` documents are authored on disk and read at runtime
through the injected `tairix_help::HelpSource`; help is never embedded in
the program (`plans/APPS.md`).

## Stability

Experimental. The socket-listing surface tracks the unfrozen `abi-v1`
`NET_SOCKETS` query and may evolve until the first release.
