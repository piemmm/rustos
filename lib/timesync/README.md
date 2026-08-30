# tairix-timesync

Stability tier: **experimental**

The clock **policy** half of TAIRiX time synchronisation: when to set the wall
clock, and what provenance to record when it is set. The NTP protocol itself
lives in `tairix-net::ntp`; this crate owns no wire format and no retry policy.

The design is binding in `plans/TIMESYNC.md`.

## Why a policy crate at all

A Raspberry Pi 3/4 has no RTC, so it boots knowing nothing and must correct
itself immediately. A Pi 5, or any machine with a battery-backed RTC, boots
with a perfectly good time and must **not** query a public NTP server just
because it rebooted. Those two requirements pull in opposite directions, and
the rule that reconciles them is a policy — not a schedule, and not something
to leave to a caller to get right.

## What it provides

- `decide` — the start-up decision, from the wall clock's reading, the
  persisted `SyncRecord`, and the configured refresh cadence. It returns
  `SyncNow(reason)` only when there is a reason to distrust the clock:
  - `ClockUnset` — no time established this boot.
  - `Implausible` — outside the `tairix_abi` plausibility window (before this
    release existed, or a century hence).
  - `WentBackwards` — earlier than the last instant time was seen at. The
    dead-RTC-battery case, whose reset date is often plausible in itself.
  - `StaleBoot` — more than `STALE_BOOT_GAP` (5 days) behind the last-seen
    instant.

  Otherwise it returns `RefreshAfter(cadence)`, measured in **uptime**, so a
  machine that reboots ten times an hour still queries once a day.
- `SyncRecord` — the persisted last-sync / last-seen pair. `last_seen` never
  moves backwards, so one bad sample cannot defeat the `WentBackwards` rule. A
  missing or unreadable store resolves to `EMPTY`, which makes the gap rules
  silent rather than firing against a guessed instant.
- `TimeSync` — the non-blocking client: the start-up decision, the
  `tairix-net::ntp` engine it drives, one folded `next_deadline()`, and the
  `ClockUpdate` it produces.
- `events` — the `23000..24000` `EventId` range the time service emits. It is
  a published contract audit-log readers and the QEMU verticals match on, so
  it lives here rather than inside the service binary's crate, which no
  consumer can depend on without taking its whole dependency tree.

## Provenance describes the source, never the size of the change

An applied sample always records `WallTimeState::Trusted`: the reading comes
wholly from the network time source, whether it established an unset clock,
replaced an RTC's, or refreshed an earlier sync. `WallTimeState::Adjusted` is
never used here — the ABI defines it as a previously-set time corrected after
the fact, which describes a manual step (the Date & Time application), not a
source replacing its own value.

A correction wider than `STEP_THRESHOLD` is reported as a *step* rather than a
refinement, and establishing an unset clock always is. That is an **audit**
distinction the caller records — a large jump can move certificate validity and
change how a reader interprets the log — and it never changes the provenance.

The kernel wall clock is set-and-project and has no frequency-correcting
primitive, so there is no gradual slew here and none is implied.

## The persisted document

`SyncRecord::to_bytes` / `from_bytes` are the on-disk encoding of
`/System/Settings/Time/state` (`RECORD_PATH`): a fixed-length, magicked,
CRC-32C-checksummed record. Every refusal — a wrong length or magic, a torn
rewrite, an undefined flag bit, an instant outside the plausibility window, a
`last_seen` earlier than its `last_sync` — resolves to `EMPTY` rather than an
error, because "we do not know when time was last seen" is exactly what a lost
record means and it makes the gap rules silent instead of wrong. The checksum
guards corruption, not tampering: a principal that can write the file can
recompute it, and only the per-inode policy on the document keeps a forged
instant out.

## Two ways in, one policy

`TimeSync::on_datagram` decodes the datagram in the caller's own address space.
A caller holding `CAP_TIME_SET` must not do that, so it evaluates the bytes in a
capability-empty sandbox worker (`lib/sandbox::timesync`) and feeds the verdict
to `TimeSync::on_reply` instead. Both reach the same clock policy, so the
containment split cannot change what a sample means.

## Purity

`no_std`, `#![forbid(unsafe_code)]`, allocation-free. Monotonic time, the
wall-clock reading, and every CSPRNG word are supplied by the caller — the same
shape as the DHCP, DNS, and TCP engines. There are no sockets and no clock
access, so the whole policy is host-tested with no kernel.
