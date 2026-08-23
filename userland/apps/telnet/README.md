# tairix-telnet

TAIRiX `telnet` — the RFC 854 Network Virtual Terminal client, in the
familiar BSD/inetutils shape (`plans/TELNET.md`, a `plans/APPS.md` command
app).

`telnet [host [port]]` opens a TCP connection and relays the terminal to it;
the escape character (`^]` by default) drops into the `telnet>` command
interpreter, and a bare `telnet` starts there. `-4`/`-6` restrict the family,
`-8`/`-L` ask for an 8-bit path, `-e`/`-E` set or remove the escape
character, `-a`/`-l` export a login name, `-b` binds a local address, and
`-d` traces the negotiation. `-?`/`--help` render the tool's own short help.

## How it reaches the network

The tool opens a `SocketType::Stream` socket through the versioned
`netsock-v1` socket ABI served by `userland/net/netstack`, gated on `CAP_NET`
and audited. A host *name* is resolved through the shared userland stub
resolver (`lib/resolver`) over an ordinary UDP socket; a literal address
needs no query at all. It never crafts a packet, never touches a device, and
holds no ambient authority.

## Protocol scope

RFC 854 NVT with RFC 885 `EOR` and RFC 1184's `EOF`/`SUSP`/`ABORT`; RFC 855
negotiation over the RFC 1143 loop-free Q Method; and BINARY (RFC 856), ECHO
(RFC 857), SUPPRESS GO AHEAD (RFC 858), STATUS (RFC 859), TIMING MARK
(RFC 860), TERMINAL TYPE (RFC 1091), NAWS (RFC 1073), TERMINAL SPEED
(RFC 1079), TOGGLE FLOW CONTROL (RFC 1080), LINEMODE (RFC 1184, in full) and
NEW-ENVIRON (RFC 1572). Every other option is refused, which is what an
unimplemented option means.

Everything the remote host sends is attacker-controlled, so the receive
parser is total, bounded (`nvt::MAX_SUBNEG_LEN`) and fails closed: an
over-long or malformed subnegotiation is discarded whole and parsing resumes
at `IAC SE`, and a peer that repeats a negotiation forever draws exactly one
answer.

Three deliberate divergences from the historical tool are documented in the
`Help/` tree and `docs/src/userland/networking.md`: no `!` shell escape (a
program parsing hostile network input is not given the authority to spawn a
shell), no `slc check` (RFC 1184 gives it no wire form distinct from
`slc export`), and a Synch that travels as the bare Data Mark because the
socket interface exposes no TCP urgent data.

## Structure

* `command` — the option grammar (`Command`/`Config`/`Target`) and its parser.
* `error` — `TelnetError`, the fatal outcomes of `run`.
* `io` — the `Output` seam.
* `net` — the `TelnetIo` seam (resolve, connect, one ordered event stream,
  send, terminal controls) and `Endpoint`/`IoEvent`.
* `nvt` — the RFC 854 command vocabulary, the incremental receive parser, and
  the transmit cooking.
* `option` — the option codes and the RFC 1143 negotiation state machine.
* `subneg` — the TERMINAL TYPE / NAWS / TERMINAL SPEED / TOGGLE FLOW CONTROL
  / NEW-ENVIRON / STATUS payload codecs, and the operator-defined `Environ`.
* `linemode` — RFC 1184: the `MODE` mask, the SLC table and its level/ack
  rules, and `FORWARDMASK`.
* `edit` — the SLC-driven local line editor, assembled from `lib/vt`'s shared
  control vocabulary and Delete-key recogniser.
* `session` — the one state machine folding both directions.
* `interp` — the `telnet>` command interpreter.
* `client` — `run`, the relay loop over the injected seams.
* `run.rs` — the freestanding `Run` binary (pure-Rust, `tairix-rt`).

The bundle's `Help/` documents are authored on disk and read at runtime
through the injected `tairix_help::HelpSource`; help is never embedded in the
program (`plans/APPS.md`).

## Why the `Run` binary has two threads

The stack delivers a socket's stream events to an async port, which joins a
wait-set; a console-backed standard input cannot join one, so the two sides
cannot be multiplexed by a single wait. Both have a real wake source, though,
and a blocking read is one — it just needs its own flow of control. So a
second thread does nothing but block in `Stdin::read` and forward what it
read to a second port, and the main thread parks on a wait-set holding both.
Neither thread ever spins and no timer is armed; the engine above the seam
stays single-threaded and host-testable.

## Stability

Experimental. The stream-socket path tracks the unfrozen `abi-v1`
`netsock-v1` contract and may evolve until the first release.
