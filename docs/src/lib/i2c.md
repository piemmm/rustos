# `tairix-i2c`

`lib/i2c` is the I²C **register-transaction protocol**: the write-then-read
composition every register-addressed part needs, over the `abi-v1` transfer
seam. It names no device, board, or chip, so it lives in `lib/*` as shared
common code (`AGENTS.md` §6 / §2.2) — *not* under the §2.20 / §2.22
single-device carve-out. Each chip driver (`drivers/rtc/ds3231`,
`drivers/rtc/pcf8523`, `drivers/rtc/pcf85063a`) composes it without a
`drivers/*` → `drivers/*` dependency (§17.4), exactly as the bus-agnostic xHCI
protocol lives in [`tairix-usb`](./usb.md) rather than the xHCI driver.

## What it provides

- **`Device`** — one register-addressed part, bound to one `I2cPort`, with
  `read`, `read_one`, `write`, `write_one`, and `update_one` (the
  read-modify-write every status-flag clear needs, which writes only when the
  value actually changed so a control register the chip owns bits in is never
  clobbered from a driver's stale idea of it).
- **`MAX_BLOCK_LEN`** — the longest register block one transaction can carry,
  *derived* from the seam's own per-phase bound minus the pointer byte, so the
  two cannot drift.
- **`mock::MockPart`** (behind the `mock-bus` feature) — the shared
  register-file part double: a seedable register file, the chip's pointer
  auto-increment, a programmable fault, and a transfer counter. Every chip
  driver's host tests drive this one definition, so no driver carries a
  private copy of the scaffold (§2.2).

It re-exports `I2cPort` and `MAX_TRANSFER_LEN` from `lib/abi`, so a chip
driver names the whole vocabulary through one crate.

## One request, not two

A register read is a *write-then-read*: the pointer write and the read-back
are one request, so no other transfer can be interleaved between them and
return some other register's contents — a wrong clock rather than an error.
`Device` never splits them, and a host test asserts the transaction's shape
rather than merely its result.

## What a chip driver holds

A `Device` is one `I2cPort`, and **a port carries no address**. On a real bus
it is the per-child transfer endpoint the bus driver serves it on, and the
address lives only in that driver's duty grant, so a chip driver has no field
in which it could name a neighbour however it is compromised.

Discovery splits each bus child in two — the bus node carries the duty
(`HwResourceKind::BusChild`, an endpoint id paired with the child's bus
address) and the child node carries the authority (a plain endpoint grant) —
which is described in full in [the RTC driver
page](../drivers/rtc.md#the-i²c-tier) and staged in `plans/TIMESYNC.md` TS-4.

## Stability

**Experimental.** The public surface is `Device` and its five register
operations, `MAX_BLOCK_LEN`, and the `mock` module.

## References

- I²C-bus specification and user manual, NXP UM10204.
