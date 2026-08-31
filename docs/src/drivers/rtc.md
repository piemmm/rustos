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
  epoch (ARM PL031, the Goldfish RTC, the Pi's PMIC clock). Nothing to decode
  beyond the width.
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

| Driver     | Crate                   | Hardware                                     | Status |
|------------|-------------------------|----------------------------------------------|--------|
| `pl031`    | `drivers/rtc/pl031`     | ARM PrimeCell PL031 (`arm,pl031`)            | shipped in the aarch64 driver store; its QEMU vertical proves the clock is set `Firmware` before any network exists |
| `goldfish` | `drivers/rtc/goldfish`  | Google Goldfish RTC (`google,goldfish-rtc`)  | shipped in the riscv64 driver store, with the same vertical over this port's own chip |
| `mc146818` | `drivers/rtc/mc146818`  | PC CMOS clock (`motorola,mc146818`)          | shipped in the x86_64 driver store, matching the node the legacy-fallback discovery path emits; its QEMU vertical waits on the first x86_64 full-boot harness |
| `rpi`      | `drivers/rtc/rpi`       | Pi 5 PMIC clock (`raspberrypi,rpi-rtc`)      | shipped in the flashable Pi driver store; host-proven against the mock firmware, and no QEMU vertical is possible because QEMU models no `VideoCore` |

The CMOS clock is the class's only port-addressed part, and the only one whose
node is a legacy fallback rather than a discovered one: no ACPI table
enumerates it and every PC-compatible machine has it at the same fixed
index/data pair (I/O ports `0x70`/`0x71`), so the x86_64 architecture port
synthesises the node unconditionally. The driver still binds by *matching*
that node, so the assumption stops in the architecture port and never reaches
the driver. Its transfers go through the capability-gated `port_read` /
`port_write` traps, which bound each access inside the granted range
kernel-side, rather than through a mapped register window — which is why
`tairix_abi::driver::sole_port_range` resolves a port grant separately from
`sole_register_window`.

The Pi's clock is the class's only part that is not addressed at all — no
window, no port pair. It lives inside the board's power-management IC, and the
`VideoCore` firmware owns it and exposes it as two numbered registers behind
the mailbox property channel, so the driver's sole path to the hardware is a
property exchange with the `vcmailbox` service over the board-neutral
`MailboxChannel` seam. Its counter reads zero until something programs it,
which is the state a board with no backup cell comes up in, so zero is the
chip's own "never set" signal: `read` answers `Ok(None)` for it and `set`
refuses to write it, so the two agree. `battery_backed` comes from the backup
cell's own voltage register, because the battery is an optional accessory on
every Pi that has this clock — a board name would not be evidence.

Some Pi firmware revisions answer every property request with the top-level
success code while never processing the tag. `lib/vcmailbox` requires the
per-tag response bit, so such a reply is reported as a fault rather than
believed as a 1970 reading, and the driver never waits on the mailbox for one.

`plans/TIMESYNC.md` TS-4 stages the remaining Raspberry Pi tier: the I²C HAT
chips, reached through a bus path (`lib/i2c` plus a BSC bus driver) that does
not exist yet.
