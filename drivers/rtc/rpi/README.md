# `tairix-drv-rtc-rpi`

The Raspberry Pi real-time-clock driver: the `raspberrypi,rpi-rtc` clock the
Pi 5 carries inside its power-management IC, and the `Firmware` rung of the
wall clock's provenance ladder on that board (`plans/TIMESYNC.md` TS-4).

Unlike every other RTC in the tree the chip is **not memory-mapped** — the
`VideoCore` firmware owns it and exposes it as two numbered registers behind
the mailbox property channel — so this driver maps nothing. Its only path to
the hardware is a property exchange with the `vcmailbox` service driver over
the board-neutral `MailboxChannel` seam.

Two targets, one crate. `src/lib.rs` is the device logic — the driver identity
(`register` + bind table) and the `Rtc` class implementation over the channel —
host-tested against the protocol-faithful `lib/vcmailbox` mock firmware.
`src/main.rs` is the `Run` binary `devmgr` autoloads into user space: it binds
`RTC_ENDPOINT` and parks in `call_recv` serving the `lib/abi` RTC wire
contract.

## Supported hardware

Any `raspberrypi,rpi-rtc` node. The Pi 3 and Pi 4 have no such node, so the
bundle ships and simply stays unbound there — the node is left unbound and
logged, which is not an error.

Two firmware registers are used, by the selectors the property interface
defines: the 32-bit Unix seconds counter, and the backup cell's voltage in
millivolts. The counter spans 1970-01-01 through 2106-02-07; an instant
outside it is refused rather than wrapped or clamped.

The counter reads **zero** until something programs it, which is the state a
board with no backup cell comes up in, so zero is the chip's own "never set
since it lost power" signal: `read` answers `Ok(None)` for it rather than
reporting 1970 as a wall time, and `set` refuses an instant of exactly zero so
nothing this driver writes can read back as "no time".

## Limitations

* **QEMU models no `VideoCore`.** There is no emulated firmware to answer a
  property request, so no QEMU vertical is possible for this driver; the
  register selection, the health decode, and every fail-closed path are proven
  host-side against the mock firmware, and the live channel is the on-metal
  acceptance item.
* **No alarm.** The alarm, alarm-pending, and alarm-enable registers the
  property interface also exposes are never touched: the driver neither arms
  nor services an alarm, and its one client polls.
* **One-second precision.** The counter holds whole seconds, so a `set` /
  `read` round trip loses the sub-second part. The declared precision says so.
* **Firmware that refuses the tag.** Some revisions answer with the top-level
  success code while never processing the tag
  (`raspberrypi/linux` issue 7230). `lib/vcmailbox` requires the per-tag
  response bit, so that reply is reported as a fault rather than believed as a
  1970 reading, and the driver never waits on the mailbox for it.
* **No plausibility judgement.** Whether a reading is a believable wall time
  is clock policy and belongs to `userland/system/timed`, the one holder of
  `CAP_TIME_SET`.

## Capabilities

`register` requires `CAP_DRV_LOAD`. The `Run` binary is granted `CAP_MAILBOX`
(to reach the `vcmailbox` service's endpoint, which the kernel gates on that
capability) and `CAP_IPC_BIND_PRIVILEGED` (to create the restricted-sender
endpoint). It needs no `CAP_MMIO_MAP`: the matched node requests no resources
and the driver maps nothing. It holds no clock authority: `RTC_ENDPOINT`
admits only senders holding `CAP_TIME_SET`, and the driver never tags a
reading's provenance itself.

## Stability

`experimental` — the protocol layer is host-proven and the on-metal channel is
the outstanding acceptance item.

## Tests

`src/tests.rs` covers the load gate, the bind table, the counter decode and
its write/read round trip, an unprogrammed counter, the backup-cell voltage as
the battery-backed signal, the pre-1970 / post-2038 / end-of-range / epoch
instants, a dead mailbox, and firmware that stamps success without processing
the tag. `lib/vcmailbox`'s own tests cover the property framing, the echoed
register selector, and the response-bit requirement beneath them.
