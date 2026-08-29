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

## Purity

`no_std`, `#![forbid(unsafe_code)]`, allocation-free, with monotonic time, the
wall-clock reading, and every CSPRNG word injected by the caller. The whole
policy is host-tested with no kernel and no sockets.
