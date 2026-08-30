# `tairix-timed` — the time-synchronisation service

Stability tier: **experimental**.

`timed` establishes and maintains the machine's wall clock. It is the **only**
principal in the system granted `CAP_TIME_SET`, so every path that sets the
clock from a network source runs through it (`plans/TIMESYNC.md`).

The installed binary lives at `/System/Services/timed.app/Run`. It is a
**boot-floor** service: a Raspberry Pi 3/4 has no RTC at all, so without this
service such a machine boots with `WallTimeState::Unset` for ever — and the
audit log's hash chains, ARXFS `Time64` metadata, and certificate lifetimes all
rest on the clock it establishes.

## What it holds, and what it deliberately does not

`CAP_TIME_SET` (the clock), `CAP_NET` (an ordinary UDP datagram socket),
`CAP_SANDBOX_SPAWN` (the response evaluation worker), `CAP_FS_ACCESS` (its
configuration and its own record), `CAP_LOG_EMIT` (its audit records).

It binds no endpoint — it is only ever a client — and holds no `CAP_NET_RAW`,
no `CAP_NET_ADMIN`, no general spawn authority, and no chown or
users-database reach.

## The packet never meets the capability

Setting the machine clock arbitrarily is a real attack: it can invalidate
certificate lifetimes, reorder how a reader interprets the audit log, and move
capability expiry. So the process holding `CAP_TIME_SET` never parses an
attacker-controlled packet. Every received datagram is evaluated inside a
capability-empty sandbox worker (`tairix_sandbox::timesync`) — this same binary,
respawned through the kernel's sandbox spawn mode holding two pipe ends and
nothing else. Three things keep the worker from mattering:

* The **nonce echo is gated here**, before the worker is involved at all, so an
  injected flood is dropped without a round trip rather than becoming a denial
  of service against the real reply.
* Only the fixed 48-byte header is copied in. Copying a fixed-length buffer is
  not parsing.
* Any sample the worker returns is **re-validated** against the plausibility
  window, the round-trip ceiling, and the usable stratum range before it can
  reach the clock.

## When it queries

The decision is `tairix_timesync::decide`, not a schedule. It queries as soon
as the network allows only when there is a *reason* to distrust the clock: it is
unset, it reads outside the plausibility window, it has gone backwards, or the
persisted last-seen record is further behind than five days. Otherwise the clock
is believed and the next query waits for the configured cadence to elapse **in
uptime** — so a machine that reboots ten times an hour still queries once a day,
which is the whole point of trusting a working RTC.

The persisted record is `/System/Settings/Time/state`, a fixed-length
checksummed document. A missing or corrupt one resolves to "time was never
seen", which makes the stale-boot and went-backwards rules simply not fire
rather than fire on a fiction.

## Configuration

Two keys in `/System/Settings/Configuration/system.conf`
(`tairix_sysconfig`):

* `time.servers` — `none` (the default) or a comma-separated list of at most
  eight host names or address literals.
* `time.refresh` — `6h`, `12h`, `1d` (the default), `2d`, or `7d`.

The default names no server. TAIRiX has no NTP-pool vendor zone, and RFC 8633
§3.1 asks a vendor not to point a fleet at the public pool without one, so a
machine that has been given no server simply never queries and says so in the
log. Naming one is the operator's or the installer's decision.

The store lives on the encrypted root and this is a boot-floor service, so its
first read normally happens before that root is mounted. There is no userland
event for "the root is mounted", so the store is re-read on a bounded, doubling
schedule folded into the reactor's own deadline — about seventeen minutes of
attempts, parking between them, never a spin and never a wait on the start-up
path. The service then either has a server or says it has none and exits;
configuring one after that means restarting the service.

A machine with no store *backing* at all (a volume-less boot) is distinguished
from one whose document is merely absent: the first arms no schedule, because
waiting cannot make a filesystem appear.

A service with nothing left to wait for — every configured server retired by a
Kiss-o'-Death, or none configured and the ladder spent — **exits** rather than
holding a task and a bound delivery port for the rest of the boot doing
nothing. Restarting it is the recovery path either way, and PID 1 audits the
exit.

## Politeness

The cadence controls live in the engine (`tairix_net::ntp`), not here: a hard
minimum poll floor configuration cannot lower, one request in flight at a time,
rotation across the configured servers, bounded exponential backoff with CSPRNG
jitter, a randomised initial delay, and Kiss-o'-Death obeyed. A request that
cannot be sent is not retried on the spot — the engine's own response timeout
ends the transaction and its backoff paces the next attempt, so a machine whose
network is not up yet sends nothing and spins on nothing.

## Tests

The whole orchestration is host-tested over injected clock, record-store, and
transport seams with the sandbox worker running in-process
(`tairix_sandbox::loopback`), so the authority split, the anti-spoof gate, the
plausibility refusal, and every audit record are covered without processes or
sockets. The end-to-end path — an unset-clock guest reaching
`WallTimeState::Trusted` from a fixture responder, and a wrong-nonce reply being
refused — is a QEMU vertical.
