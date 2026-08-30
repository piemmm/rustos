# Real-time-clock drivers

A real-time clock is the machine's *local* wall-time source: a counter the
board keeps across a power cycle, so a machine has a plausible time before any
network exists. It supplies the `Firmware` rung of the wall clock's provenance
ladder — believed until a validated network sample replaces it, never the
other way round.

The staged design is `plans/TIMESYNC.md`; this page is the driver-class view.

## The authority split

An RTC driver holds **no** clock authority. It reads and writes its own chip
and nothing else; the machine clock is set by the single holder of
`CAP_TIME_SET` (`userland/system/timed`), which reads the chip over the RTC
service endpoint and tags the reading itself.

That split is what makes the provenance ladder enforceable. `wall_time_set`
takes its provenance from the caller, so a driver holding the clock capability
could simply assert `Trusted` and the ladder would be worthless. As it stands,
the worst a compromised RTC driver can do is lie about a time that the kernel
will not let overwrite a network sync.

The kernel enforces the ladder itself rather than trusting a driver to be
polite: `WallTimeState::supersedes` refuses a `Firmware` write over a
`Trusted` or `Adjusted` reading, and `wall_time_set` answers
`Errno::AlreadyExists`. Rolling a clock backwards is how an expired
certificate is revived and how audit reasoning is reordered, so the refusal is
structural.

## Class trait

`tairix_abi::driver::rtc::Rtc` is three methods:

| Method   | Purpose                                        | Capability gate          |
|----------|------------------------------------------------|--------------------------|
| `status` | the chip's live precision and health flags     | `DriverHandle` ownership |
| `read`   | the instant it can vouch for, if any           | `DriverHandle` ownership |
| `set`    | write an instant, clearing its stopped flag    | `DriverHandle` ownership |

`read` answers `Ok(None)` — not an error — when the chip cannot vouch for its
counter: its oscillator stopped, its clock-integrity flag is set, or its
registers hold no real calendar date. A board with a flat backup cell is an
ordinary state, and a driver must never substitute an epoch, a build date, or
any other fabricated instant. `RtcStatus::oscillator_stopped` is what lets a
consumer report *why* it has no time.

`RtcStatus::battery_backed` is a claim about the board, so a driver that
cannot demonstrate it reports `false`: understating is safe, because a
consumer that trusts a reading less is never harmed by it.

Judging whether a reading is a *believable* wall time is clock policy and
belongs to `timed`, not to a driver.

## Two register shapes, one codec

Concrete chips divide into two families:

* **Linear counters** — a seconds or nanoseconds count since a documented
  epoch (ARM PL031, the Goldfish RTC). Nothing to decode beyond the width.
* **Calendar register blocks** — packed binary-coded-decimal fields for
  second/minute/hour/day/month/year (MC146818 CMOS, DS3231, PCF8523,
  PCF85063A).

The BCD conversion and the bridge to `tairix_abi::time::CivilTime` are the
same for every chip in the second family, so they are defined once in the
class module (`bcd_to_bin`, `bin_to_bcd`, `resolve_two_digit_year`) and each
driver contributes only its own register offsets, century handling, and
quirks. A chip storing a two-digit year has one interpretation per century;
`resolve_two_digit_year` picks the one inside the same fixed plausibility
window the wall clock already validates against, so a resolved year is one the
clock would have accepted anyway.

## The service endpoint

A driver serves `tairix_abi::rtc_ipc::RTC_ENDPOINT`, a synchronous call
endpoint bound restricted-sender under `CAP_TIME_SET` — the only principal
with a reason to read or write the board's clock chip is the one that sets the
machine clock from it, so an existing capability expresses the authority
exactly and no new one is defined.

The id is a single reserved value rather than a per-instance slot range: every
board TAIRiX targets exposes one RTC, and a second would need a *selection
policy* that no consumer has. A second driver's `call_create` therefore fails
closed with `Errno::AlreadyExists`, which it logs and exits on, so the outcome
is the first RTC in hardware-tree order and the situation is visible in the
log rather than silently arbitrary.

`timed` reads the endpoint at start-up. Because it is a boot-floor service and
the driver is autoloaded, the endpoint is usually not bound on the first
attempt; there is no userland event for "that driver bound", so the read
climbs a bounded, doubling one-shot ladder folded into the reactor's own
deadline — never a spin, and finite, so a board with no clock chip stops
asking. After a validated network sync, `timed` writes the instant back to the
chip, so a machine that syncs once then boots offline still starts from a good
time.

## Shipped drivers

| Driver  | Crate                     | Hardware                              | Status |
|---------|---------------------------|---------------------------------------|--------|
| `pl031` | `drivers/rtc/pl031`       | ARM PrimeCell PL031 (`arm,pl031`)     | device logic and service complete; image wiring and its QEMU vertical are the next increment |

`plans/TIMESYNC.md` TS-3 stages the MC146818 CMOS clock (x86_64) and the
Goldfish RTC (riscv64) beside it, and TS-4 the Raspberry Pi tiers.
