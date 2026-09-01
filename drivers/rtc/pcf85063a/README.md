# `tairix-drv-rtc-pcf85063a`

Autoloaded user-space real-time-clock driver for the **NXP PCF85063A**, an I²C
calendar chip at bus address `0x51`.

## Supported hardware

Device-tree `compatible`: `nxp`, `pcf85063a`. The
driver binds only through the discovery match (`BIND_KEYS`); it names no board
and never reaches for an address of its own.

## Required capabilities

| Capability | Why |
|---|---|
| `CAP_DRV_LOAD` | the load-time gate every driver clears |
| `CAP_IPC_ENDPOINT` | calling the transfer endpoint its matched node's grant names |
| `CAP_IPC_BIND_PRIVILEGED` | binding the RTC service endpoint it serves |

It requests **no** `CAP_MMIO_MAP`: it owns no registers of its own, only a
path to its part. It holds no clock authority either — the machine clock is
set by the one holder of `CAP_TIME_SET` (`userland/system/timed`), which
reads this driver's RTC service endpoint and tags the reading `Firmware`
itself. A compromised RTC driver can lie about the *chip*, but it can neither
assert a provenance it did not earn nor overwrite a network sync.

## One endpoint, one part

Discovery splits each bus child in two: the bus driver receives the **duty**
(the endpoint id paired with this part's bus address) and this driver receives
the **authority** (the endpoint alone). The transfer wire carries no address,
so this driver has no field in which it could name a neighbour on the bus. A
driver delivered no endpoint grant stands down rather than guessing an id.

A chip driver whose bus driver has not yet bound its endpoint simply gets a
refusal from the first call; `timed`'s existing bounded RTC ladder covers the
start-up ordering, so nothing here retries or spins.

## What it can and cannot vouch for

The seconds register's top bit is the chip's own clock-integrity flag: set
whenever the oscillator has stopped, and cleared only by the write that puts a
real time in the counter. While it is set the calendar registers mean nothing,
so `read` answers `Ok(None)`.

Bring-up puts the part in **24-hour** mode with its time circuits running,
because both live in a control register rather than in the calendar block. It
is a read-modify-write, so the board's other settings survive. The read path
still honours a 12-hour field, because the mode bit and the field can
legitimately disagree in the window before the first write.

It is the PCF8523's smaller sibling and shares that part's *shape*, but not its
register map — the block base, the hour-format bit, and the battery reporting
all differ — so the two are separate drivers rather than one behind a
conditional.

The part has **no battery-switch-over circuit and no backup-cell input** of its
own — a board that wants persistence supplies backup power to the whole part —
so there is nothing in the chip to read and `battery_backed` is reported
`false`. That is the conservative direction: a consumer then treats the reading
as no older than this boot, and the integrity flag tells it if even that is
untrue.

Judging whether a *decoded* time is a believable wall time is clock policy and
belongs to `timed`. The one exception is the two-digit year, which names one
year in every hundred: it resolves through the class's shared
`resolve_two_digit_year`, against the same fixed window the wall clock
validates every reading against. An instant outside that window is **refused**
on write rather than stored as a year that would read back as another century.

## Testing

Host unit tests drive the chip against `tairix_i2c::mock::MockPart`, the
shared register-file part double — one definition every I²C chip driver's
tests use, so no driver carries a private copy of the scaffold. They cover the
24-hour and 12-hour register layouts, a lost clock integrity, a register block
that is not a calendar, a date the calendar does not have, the write/read
round trip, a failed write leaving the chip unable to vouch, and every year
the plausibility window admits.

**QEMU does not model the Broadcom Serial Controller** these parts hang off,
so no integration test is possible for this tier: no emulated machine in the
matrix presents an I²C bus with a clock chip on it. The decode, the encode,
and every fail-closed path are host-proven and the live part is an on-metal
acceptance item, as for the rest of `plans/PI.md`'s Raspberry Pi work.

## References

- NXP PCF85063A data sheet, register overview.
- I²C-bus specification and user manual, NXP UM10204.
- `plans/TIMESYNC.md` TS-4 — the staged design this driver lands.
