# The time service (`timed`)

`timed` establishes and maintains the machine's wall clock. It is the only
principal in the system granted `CAP_TIME_SET`, so every path that sets the
clock from a network source runs through it. The staged design is
`plans/TIMESYNC.md`; the clock policy it drives is
[`tairix-timesync`](../lib/timesync.md) and the wire protocol is
`tairix_net::ntp`.

The installed binary lives at `/System/Services/timed.app/Run` and is a
boot-floor service PID 1 launches from its startup configuration. A Raspberry
Pi 3/4 has no RTC at all, so without this service such a machine boots with
`WallTimeState::Unset` for ever — and the audit log's hash chains, ARXFS
`Time64` metadata, and certificate lifetimes all rest on the clock it
establishes.

## The authority split

Setting the machine clock arbitrarily is a real attack: it can invalidate
certificate lifetimes, reorder how a reader interprets the audit log, and move
capability expiry. The process holding `CAP_TIME_SET` therefore never parses an
attacker-controlled packet. Every received datagram is evaluated inside a
capability-empty sandbox worker (`tairix_sandbox::timesync`) — this same binary
respawned through the kernel's sandbox spawn mode, holding two pipe ends and
nothing else.

Three properties keep a compromised worker from mattering:

* The **nonce echo is gated caller-side**, before the worker is involved at all.
  The origin timestamp sits at a fixed offset in a fixed-length header, so
  reading it is a field extraction rather than a parse, and an injected flood
  is dropped without a round trip instead of becoming a denial of service
  against the real reply.
* Only the fixed 48-byte header crosses the boundary. A longer datagram's tail
  is what the codec ignores anyway, and copying a fixed-length buffer is not
  parsing.
* Any sample the worker returns is **re-validated** against the plausibility
  window, the round-trip ceiling, and the usable stratum range before it can
  reach the clock.

The engine's transaction machine is reached through
`NtpClient::on_reply`/`TimeSync::on_reply`, which take an already-evaluated
verdict, so the retry, rotation, and Kiss-o'-Death discipline has exactly one
implementation whether or not the decode was sandboxed.

## Capabilities

`CAP_TIME_SET`, `CAP_NET`, `CAP_SANDBOX_SPAWN`, `CAP_FS_ACCESS`,
`CAP_LOG_EMIT` — the service account's `TIMED_CEILING` carries exactly the same
five, so the kernel's `manifest ∩ ceiling` grant strips nothing the code needs.

It binds no endpoint (it is only ever a client) and holds no `CAP_NET_RAW`, no
`CAP_NET_ADMIN`, no general spawn authority, and no chown or users-database
reach. Compromising it yields the machine clock — which is precisely why the
packet parsing is not in it.

## Which servers, from where

Three tiers, worst to best, with the first non-empty one winning outright
(`tairix_timesync::select_servers`):

| Tier | Where it comes from |
|---|---|
| `fallback` | The built-in `0.pool.ntp.org` … `3.pool.ntp.org`. |
| `network` | DHCPv4 option 42 / DHCPv6 option 56, learned by `netstack` from the current lease and published through the ungated `net_time_servers` system-information query. |
| `configured` | The `time.servers` key the operator or the installer wrote. |

The tiers are never merged: a machine whose network named a server must not
keep querying the public pool as well, and an operator who named one must not
have a DHCP server's choice mixed into their list. The set is therefore never
empty, so a stock installation keeps time with no configuration at all — the
`fallback` tier exists precisely because a machine that asks nobody has no
clock.

A network-supplied server arrives *as an address*, so it needs no name
resolution: a machine whose only DNS advice would have come from the same lease
still keeps time. The `state:net/time/servers` read shows what the network
offered, and the service's own start-up audit record carries a `source` field
naming the tier actually in use.

RFC 8633 §3.1 asks a vendor shipping a fleet to obtain its own NTP-pool vendor
zone rather than point every device at the generic names; TAIRiX has no such
zone yet, so the generic names are what it can honestly use. What makes that
acceptable is the politeness policy the engine enforces on every tier alike — a
hard minimum poll interval, one request in flight per server, bounded
exponential backoff with CSPRNG jitter, a randomised initial delay, and
obedience to a Kiss-o'-Death — plus the pool's own DNS rotation. Registering a
vendor zone changes only `FALLBACK_TIME_SERVERS`.

## Configuration

Two keys in `/System/Settings/Configuration/system.conf`:

* `time.servers` — `none` (the default) or a comma-separated list of at most
  eight host names or address literals. `none` does not mean "never query": it
  means the operator expressed no preference, so a lower tier above applies.
* `time.refresh` — `6h`, `12h`, `1d` (the default), `2d`, or `7d`: how much
  *uptime* passes between steady-state re-queries.

The store lives on the encrypted root and this is a boot-floor service, so its
first read normally happens before that root is mounted. There is no userland
event for "the root is mounted", so the tiers are re-selected on a bounded,
doubling schedule folded into the reactor's own deadline — about seventeen
minutes of attempts, parking between them, never a spin and never a wait on the
start-up path, which would hold the boot up behind a service nothing else is
waiting for. The same schedule is what picks up a DHCP lease acquired after the
service started, so the two "it is not there yet" problems have one mechanism.
A failed read never disarms it: before the root is mounted the path has no
backing at all, which is indistinguishable from a volume-less boot, so the
service would strand itself exactly when it most needs to retry. The ladder
being finite is what bounds the volume-less case instead.

Only a *strictly better* tier replaces the servers in use. An equal-tier change
— a renewed lease naming a different server — would reset the engine's rotation
and backoff and forget which servers had refused, which costs more than it
buys; the ladder's finite length bounds that churn instead. Reaching the
`configured` tier disarms the ladder outright, there being nothing better to
look for.

A server name resolves through the shared literal-first host-operand policy
(`lib/resolver`), so an address literal works with no resolver configured at
all; the stub-resolver transport is opened only when a name actually needs it.

## The persisted record

`/System/Settings/Time/state` is a fixed-length, CRC-32C-checksummed document
holding the last successful sync instant and the latest instant the machine is
known to have observed. It is the only input that can distinguish "powered off
for an hour" from "powered off for a month", which no clock reading alone can
tell.

A missing, short, long, wrongly-magicked, torn, implausible, or
invariant-violating document resolves to "time was never seen", which makes the
stale-boot and went-backwards rules simply not fire rather than fire on a
fiction. The checksum guards corruption, not tampering: a principal that can
write the file can recompute it, and only the per-inode policy on the document
keeps a forged instant out.

## The reactor

One wait-set over the delivery port the network stack posts the service's
datagrams to, armed with the timeout the engine's single folded deadline
implies. The loop parks; it never polls. A wake is either a datagram (evaluate
it in the worker, apply the verdict) or the deadline lapsing (send the next
request). With no deadline left — every server in use retired by a
Kiss-o'-Death and the re-selection ladder spent — the service exits rather than
holding a task and a bound delivery port doing nothing.

A request that cannot be sent is not retried on the spot: the engine's own
response timeout ends the transaction and its bounded backoff paces the next
attempt, so a machine whose network is not up yet sends nothing and spins on
nothing.

## Audit records

The service owns the `23000..24000` event range, whose identifiers are defined
in `lib/timesync`'s `events` module so an audit-log reader can match on them
without depending on the service crate. Every clock change carries the
applied instant, whether it was a step or a refinement, the correction
magnitude where there was a previous reading, the measured round trip, and the
server's stratum; every refused sample, retired server, rate limit, failed
evaluation, unsent query, and unwritten record has its own stable id. A spoofed
datagram is deliberately *not* audited per packet — that would make an injected
flood a log-flood denial of service — and is instead invisible in exactly the
way the nonce gate makes it irrelevant.

## Tests

The whole orchestration is host-tested over injected clock, record-store, and
transport seams with the sandbox worker running in-process, so the authority
split, the anti-spoof gate, the plausibility refusal, the kernel-refusal path,
and every audit record are covered without processes or sockets. The
`fuzz_sandbox` harness fuzzes the evaluation surface from both directions
(hostile datagrams into the worker, hostile verdicts back at the caller).

The end-to-end path is two QEMU verticals. `tairix-test-timed-qemu-aarch64`
covers the `configured` tier: an unset-clock guest, a fixture responder that
answers each request **spoof first**, and a serial witness requiring the
*exact* instant the truthful reply carried — so a guest that believed the
spoof, or that let the spoof cancel its outstanding transaction, fails the run
rather than passing it. `tairix-test-timed-dhcp-qemu-aarch64` covers the
`network` tier with **no** configured server at all: the peer leases an address
carrying DHCP option 42 that names itself, and only a guest that read that
option finds a reachable server — the fallback names cannot resolve on an
isolated wire, so a guest that ignored it never sets its clock and the run
fails loud.
