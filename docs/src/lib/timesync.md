# `lib/timesync` — the clock-setting policy

`tairix-timesync` decides **when** a machine should set its wall clock from the
network, and what provenance to record when it does. The NTP protocol lives in
[`lib/net`](net.md) as `tairix_net::ntp`; this crate holds the policy that
drives it.

The binding design is `plans/TIMESYNC.md`.

## The problem

Nothing sets the wall clock on a fresh TAIRiX boot, so the kernel starts at
`WallTimeState::Unset` and every timestamp the system writes — audit-log hash
chains, ARXFS `Time64` metadata, certificate validity — rests on a clock a
human has set by hand. A Raspberry Pi 3/4 has no RTC to seed it from at all.

Two requirements pull against each other:

- A machine with no clock must correct itself as soon as it can.
- A machine with a good clock must not query a public NTP server on every
  reboot, because that is abusive at fleet scale.

## The decision

`decide` takes the clock's current reading, the persisted `SyncRecord`, and the
configured refresh cadence, and returns the first match:

| Condition | Result |
|---|---|
| No time established this boot | `SyncNow(ClockUnset)` |
| Reading outside the plausibility window | `SyncNow(Implausible)` |
| Reading earlier than the last-seen instant | `SyncNow(WentBackwards)` |
| Reading more than `STALE_BOOT_GAP` (5 days) past the last-seen instant | `SyncNow(StaleBoot)` |
| Anything else | `RefreshAfter(cadence)` |

The plausibility window is `tairix_abi::is_plausible_wall_time`: at or after
`RELEASE_EPOCH_SECS` and no more than `PLAUSIBLE_FUTURE_SECS` (a century)
beyond it. Both are fixed validation bounds, defined once in `lib/abi` and
mirrored into the generated C header, never widened to admit a reading.

`WentBackwards` is the case a clock reading alone cannot reveal: a dead RTC
battery resets to a date that is often plausible in itself, and only the
persisted record shows that time has apparently run backwards.

The refresh cadence is measured in **uptime**, not wall time. That is what
keeps a frequently-rebooting machine from re-querying on every boot while still
refreshing about once a day.

## Which servers

`select_servers` is the other half of the policy: **whose** server to ask.
Three tiers, worst to best, first non-empty wins outright:

| `ServerSource` | Where the servers come from |
|---|---|
| `Fallback` | `FALLBACK_TIME_SERVERS` — `0.pool.ntp.org` … `3.pool.ntp.org`. |
| `Network` | What DHCP offered (option 42 / option 56), read through the ungated `net_time_servers` system-information query. |
| `Configured` | The `time.servers` key an operator or installer wrote. |

The tiers are never merged. A machine whose network named a server must not go
on querying the public pool as well, and an operator's list must not have a
DHCP server's choice mixed into it. `ServerSource` is `Ord`ered worst-to-best so
a caller compares tiers instead of re-deriving the precedence: a set may be
replaced by one of a strictly greater source and never by an equal or lesser
one, which is what stops a re-selection resetting a running engine's rotation
and backoff for no gain.

The result is never empty, because a machine that asks nobody has no clock —
which is why the fallback tier exists at all. Each tier is truncated to
`MAX_TIME_SERVERS`, the same bound the engine's server array and the
configuration store use, so a named server can never sit silently past the
engine's reach.

A `Network` server arrives as an address and carries it, so it needs no name
resolution: a machine whose only DNS advice would have come from the same lease
still keeps time. Its `name` is then the address's text, held for the audit
trail and never parsed back.

The fallback names the generic public pool because TAIRiX has no NTP-pool
vendor zone; RFC 8633 §3.1 would prefer one. What makes it acceptable is the
politeness policy `tairix_net::ntp` enforces on every tier alike — the poll
floor, one request in flight per server, bounded backoff with CSPRNG jitter, a
randomised initial delay, and Kiss-o'-Death obedience — plus the pool's own DNS
rotation.

## The persisted record

`SyncRecord` holds `last_sync` and `last_seen`, and is written to
`/System/Settings/Time/state` on each successful sync. `last_seen` never moves
backwards, so a single bad sample cannot defeat the `WentBackwards` rule. A
missing or unreadable store resolves to `SyncRecord::EMPTY`, which makes the
gap rules silent rather than firing them against a fabricated instant.

## Provenance

An applied sample always records `WallTimeState::Trusted`, because the new
reading comes wholly from the network time source. `WallTimeState::Adjusted` is
deliberately unused: the ABI defines it as a previously-set time corrected
after the fact so that the offset is no longer its original source's, which
describes a manual step rather than a source replacing its own value.

A correction wider than `STEP_THRESHOLD`, and any establishment of an unset
clock, is reported as a *step* in the `ClockUpdate` the caller audits. That is
an audit classification only — a large jump can move certificate validity and
change how a reader interprets the log — and it never changes the provenance.

The kernel wall clock is set-and-project, with no frequency-correcting
primitive, so a gradual slew is out of scope and is not implied by any state
this crate reports.

## The persisted document

`SyncRecord::to_bytes` / `from_bytes` encode
`/System/Settings/Time/state` (`RECORD_PATH`): a fixed-length, magicked,
CRC-32C-checksummed record. A wrong length or magic, a torn rewrite, an
undefined flag bit, an instant outside the plausibility window, or a `last_seen`
earlier than its `last_sync` all resolve to `SyncRecord::EMPTY` rather than an
error — "we do not know when time was last seen" is exactly what a lost record
means, and it makes the stale-boot and went-backwards rules silent instead of
wrong. The checksum guards corruption, not tampering.

## Two ways in, one policy

`TimeSync::on_datagram` decodes in the caller's address space. A caller holding
`CAP_TIME_SET` must not, so it evaluates the bytes in a capability-empty sandbox
worker ([the sandbox seam](../security/sandbox.md)) and feeds the verdict to
`TimeSync::on_reply`. Both reach the same clock policy, so the containment split
cannot change what a sample means.

## Purity

`no_std`, `#![forbid(unsafe_code)]`, with monotonic time, the wall-clock
reading, and every CSPRNG word injected by the caller. The decision path is
allocation-free; only `select_servers` allocates, because its result is an
owned server list the caller keeps. The whole policy is host-tested with no
kernel and no sockets.
