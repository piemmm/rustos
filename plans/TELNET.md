# TELNET.md — the Network Virtual Terminal client (RFC 854 / RFC 1184)

Staged build plan for TAIRiX's `telnet` command. **Binding under `AGENTS.md`**
(read it first, especially §2, §5, §16.7, §19, §24, §26); it consumes the seams
`plans/NETWORK.md` and `plans/DNS.md` fix and never contradicts them — where
they touch, NETWORK.md's decisions stand. `abi-v1` is not frozen (PLAN.md
Stage 1), but this plan adds **no** ABI surface at all.

`plans/NETWORK.md` §9 defers every consumer of the socket ABI to "its own
plan"; this is the plan for the interactive one. `plans/NETWORK.md` N5c landed
TCP streams and N15 the half-close, and `plans/DNS.md` DNS2/DNS3 landed the
stub resolver and its `lib/resolver` client — so a terminal-relaying client has
every seam it needs the moment it lands.

## 0. Scope and decisions (binding)

- **A client, not a service. There is no `telnetd` here.** The tool originates
  one ordinary TCP connection and relays a terminal over it. A telnet *server*
  would mean exposing a shell to the network, which is a different plan with a
  very different threat model, and none is speculated here (§2.3/§2.4). The
  QEMU vertical's host-side server is test-only and lives beside its one
  consumer in `tools/xtask` (the `plans/DHCP.md` D3 precedent).
- **No new capability, no new ABI, no netstack change.** `telnet` is an
  unprivileged consumer of the landed socket surface: `CAP_NET` for the stream
  socket and the resolver's UDP socket, `CAP_CONSOLE_READ`/`CAP_CONSOLE_WRITE`
  for the terminal it relays, `CAP_FS_ACCESS` for its own `Help/` tree.
  Connecting *to* a well-known port is unprivileged; only *binding* one needs
  `CAP_NET_BIND_PRIVILEGED`, and `-b` binds an ephemeral local port. Reaching
  a remote host is ordinary transport use, so no `CAP_NET_RAW` (§5.2
  minimalism).
- **The remote host is hostile (§26.4).** Every byte it sends is
  attacker-controlled *and some of them draw a reply*, so the codec is total,
  bounded (`nvt::MAX_SUBNEG_LEN`, a fixed validation bound per §24.4), fuzzed
  (§19.6), and fails closed: an over-long or malformed subnegotiation is
  discarded whole and parsing resumes at `IAC SE`. Negotiation uses the
  RFC 1143 Q Method, so a peer that repeats itself forever draws exactly one
  answer and no exchange can cycle — the amplification property the fuzz
  harness asserts directly.
- **The option set is closed, and a subnegotiation needs its option enabled.**
  Anything outside the implemented set is refused, which is precisely what an
  unimplemented option means in RFC 855; a client that silently accepted one it
  cannot honour would be lying to the server. And RFC 855 allows a
  subnegotiation only for an *enabled* option, which the client enforces —
  without that gate a server that never asked could make the client disclose
  the operator's exported `NEW-ENVIRON` variables, its terminal type and its
  window size purely on request.
- **One engine, host-testable, in the app crate.** The whole protocol —
  including the interactive relay and the `telnet>` interpreter — runs against
  injected seams, so the tested code and the shipped code are the same code.
  It stays in `userland/apps/telnet` rather than a `lib/telnet` crate because
  it has exactly one consumer (§2.3, the `ping`/`host` precedent); the QEMU
  peer imports the crate's *public wire vocabulary* rather than restating it.
- **Event-driven, never polled (§2.23).** See §2 below: the two-thread shape
  of the `Run` binary is the *reason* nothing polls, not a convenience.
- **Not in this plan:** a telnet server, TN3270, `rlogin` emulation, encryption
  or authentication options (RFC 2941/2946 — TLS fronts sockets under
  `lib/crypto` and is not a telnet option here), and a trace file. Each is
  absent rather than stubbed.

## 1. Target architecture (binding)

`userland/apps/telnet`, a `plans/APPS.md` command app, `no_std` + `alloc`,
`#![forbid(unsafe_code)]`:

- `nvt` — the RFC 854 command vocabulary (plus RFC 885 `EOR` and RFC 1184
  `EOF`/`SUSP`/`ABORT`), the incremental receive parser, and the transmit
  cooking (`IAC` doubling; `CR` to the configured line terminator).
- `option` — the option codes and the RFC 1143 negotiation state machine, with
  the closed supported set. `ECHO` is accepted from the server and never
  offered: a client that echoed on the server's behalf would double every
  character.
- `subneg` — TERMINAL TYPE (RFC 1091), NAWS (RFC 1073), TERMINAL SPEED
  (RFC 1079), TOGGLE FLOW CONTROL (RFC 1080), NEW-ENVIRON (RFC 1572) and
  STATUS (RFC 859) payloads, and the operator-defined `Environ`.
- `linemode` — RFC 1184 in full: the `MODE` mask and its acknowledgement
  discipline, the SLC table with the §3 level/ack rules, and `FORWARDMASK`.
- `edit` — the SLC-driven local line editor `EDIT` mode requires, *assembled*
  from `lib/vt`'s shared control vocabulary and Delete-key recogniser exactly
  as `lib/tty` is, so telnet agrees with the console about which keystroke rubs
  one character out. It is not a parameterised `lib/vt::LineEditor`: that
  editor's erase set is fixed and shared with the kernel console reader and
  `login`, and threading a server-negotiated table into it would put telnet
  policy in a crate the console links.
- `session` — the one state machine folding both directions into three drained
  buffers (wire, terminal, trace). No I/O.
- `interp` — the `telnet>` command interpreter, with BSD's unambiguous-prefix
  abbreviation.
- `client` — the relay loop over the injected seams, command mode included.
- `run.rs` — the freestanding `Run` binary.

## 2. Why the `Run` binary has two threads

Telnet must carry the keyboard and the connection at once. The stack delivers a
socket's stream events to an async port, which joins a wait-set; a
*console-backed* standard input cannot, because the wait-set's stream source
admits only a pipe or pty backing and a console-backed standard stream is not
in the process's open-file table at all. The two sides therefore cannot be
multiplexed by one wait.

Both do have a real wake source, and a blocking read is one — it just needs its
own flow of control. So the binary spawns one thread that does nothing but
block in `Stdin::read` and forward what it read to a second port, and the main
thread parks on a wait-set holding *both* ports. Neither thread ever spins and
no timer is armed, which is what §2.23 asks for; the engine above the seam sees
one ordered event stream and stays single-threaded and host-testable.

The alternative considered and rejected was a timed poll of standard input
between socket waits: §2.23 permits a periodic re-poll only where an event has
*no* wake source, and here it has one. Adding a kernel wait-source for
console-backed input would be the other structural answer, but that is a
kernel + ABI change well beyond a command app; it is recorded here as the
option a future increment may take, not as work this plan deferred.

## 3. Deliberate divergences (§16.7)

Each is documented in the tool's own `Help/` tree and in
`docs/src/userland/networking.md`, never silently different:

- **No `!` shell escape.** Giving a program that parses hostile network input
  the authority to spawn a shell inverts the §19.5 minimum-capability posture
  the tool is built on. The operator suspends with `z` or opens another
  terminal.
- **No `slc check`.** RFC 1184 gives it no wire form distinct from
  `slc export`, so offering both would be two names for one action (§2.3).
- **No `-n tracefile`, `-r` (rlogin), `-c` (`.telnetrc`), `set tracefile`.**
  The facilities they name do not exist (no trace file, no rlogin, no `/etc`).
  They are *unknown options*, not accepted-and-ignored ones: a switch that
  silently does nothing is worse than one that is honestly refused.
- **A Synch is the bare Data Mark.** `netsock-v1` exposes no TCP urgent data,
  so there is nothing in flight to discard ahead of it. The client still sends
  the Data Mark in band, which a server scanning for it still finds.
- **A resize is reported at the next keystroke.** TAIRiX has no window-change
  signal, so there is no event to park on; the grid is re-read on every
  keyboard event, which is a human-rate event, so NAWS is accurate as soon as
  the operator types.
- **A server-sent `AYT` is answered on the operator's screen, not the wire.**
  Injecting bytes into the server's own input stream is not a client's place.
- **End of standard input half-closes rather than exiting.** The historical
  tool exits, which discards whatever the server was still sending:
  `telnet host 80 < request` loses the response. Closing only the write side is
  what a TCP client does, so the response arrives and the peer's own close ends
  the session. A write side that is already gone still ends it at once, so a
  dead connection never parks the tool.

## 4. Staged increments

Status marks: `[ ]` planned, `[~]` in progress, `[x]` done.

### T1 — the client, its bundle, and the live vertical `[x]`

Landed as one increment: the tool is not usable in halves, and every layer of
it is host-tested at the seam below it, so splitting it would have shipped a
protocol engine with no consumer (§2.19).

Key facts for the next worker:

- **The bundle** is `userland/apps/telnet` (`tairix-telnet`), requesting
  `CAP_CONSOLE_WRITE`/`CAP_CONSOLE_READ`/`CAP_FS_ACCESS`/`CAP_NET`, with a
  hand-authored SVG icon in the `lib/svg` subset and a thirteen-locale `Help/`
  tree. Registered in the workspace, the kernel `program_manifests` pin
  (`TELNET_TOOL_REQUEST`), and the harness bundle-discovery pin.
- **Coverage** is 250 host tests across the ten modules plus the `fuzz_telnet`
  harness (registered in `cargo xtask fuzz`), whose invariants are: the parser
  never panics on any bytes however chunked; its held subnegotiation never
  exceeds `MAX_SUBNEG_LEN`; a live session's reply is bounded by its input
  rather than amplifying it; **every byte the session emits re-parses as
  well-formed telnet and leaves the stream on a command boundary**; and the
  SLC fold answers at most one triplet per triplet received.
- **The `en-US` `OPTIONS` list is pinned to the parser** by
  `command::tests::every_option_key_is_covered`, and `cargo xtask help-lint`
  pins every translation's switch keys to that canonical set.
- **The live vertical** is `tests/integration/netstack_telnet_qemu_aarch64`,
  which boots the production aarch64 pipeline against the **shared**
  `FsDisk::NetToolRootDisk` — renamed in place from `PingRootDisk`, since the
  disk (standard signed store bundles + the signed virtio-net driver) is
  exactly what any shipped-network-tool vertical needs and the two now share
  the one builder (§2.2, §2.13).
- **The host peer** (`NetPeerMode::V6TelnetServer` →
  `netpeer::telnet_server`) is a *checker* as much as a server: it records
  every step of the exchange and its verdict names the first one the client
  failed. Its strongest host test runs the **real client `Session`** against it
  and asserts the whole negotiation, the probe round trip, and the client's
  display of the answer — so a drift between the two ends fails in
  milliseconds rather than on a QEMU boot.
- **The vertical's three serial gates** are what make it a proof: the peer's
  banner is sent *only* after the client accepted `DO SUPPRESS GO AHEAD`,
  named its terminal type, reported NAWS, agreed `WILL LINEMODE`, stated a
  `MODE` mask and exported its SLC table; the peer's upper-cased answer to the
  typed probe proves a full round trip (the client's own local echo is lower
  case, so it cannot be mistaken for it); and the session is left through the
  default escape character and the interpreter's `quit`, so both are exercised
  live. PASS keys on `telnet`'s audited `exit` then the shell's, **and** the
  peer's verdict — neither side passes alone.

### T2 — `telnet` over TLS-fronted transports `[ ]`

Not started, and deliberately not designed here. When `lib/crypto`'s TLS
surface fronts a stream socket, a `telnets`-class transport becomes an
in-place extension of `TelnetIo::connect` rather than a second client (§2.13).
It is named only so a future worker knows where it would go.

## 5. Tests, docs, and gate (binding)

Every increment lands its unit/fuzz/QEMU tests, updates its rustdoc +
`docs/src/userland/networking.md` + this plan's status marks in the same change
(§13, status only — no landing narrative), and ends with the full §2.15 gate
(`cargo fmt --all`, `cargo xtask ci` once, `cargo xtask fuzz --secs 5`,
`tools/ci/soak.sh both --secs 20`), quoted in the completion report.

## 6. What this plan explicitly does *not* do

- No telnet **server**, and no exposure of a shell to the network.
- No RFC 2941/2946 authentication or encryption options; TLS fronts sockets
  under `lib/crypto` and is not a telnet option here.
- No TN3270, no `rlogin`, no trace file, no `.telnetrc`.
- No second DNS path: a host name is resolved by `lib/resolver` or not at all.
