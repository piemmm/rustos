# `tairix-drv-rtc-goldfish`

The Google Goldfish real-time-clock driver: the `google,goldfish-rtc` counter
the QEMU riscv64 `virt` board exposes, and the `Firmware` rung of the wall
clock's provenance ladder on that target (`plans/TIMESYNC.md` TS-3).

Two targets, one crate. `src/lib.rs` is the device logic — the §8 driver
identity (`register` + bind table) and the `Rtc` class implementation over a
granted `RegisterWindow` — host-tested against a mock register block.
`src/main.rs` is the `Run` binary `devmgr` autoloads into user space: it maps
its granted window, binds `RTC_ENDPOINT`, and parks in `call_recv` serving the
`lib/abi` RTC wire contract.

## Supported hardware

Any `google,goldfish-rtc` node whose register window the kernel granted the
process. The register block is a 4 KiB page; the driver touches only the
counter pair at its base — `TIME_LOW` (`0x00`) and `TIME_HIGH` (`0x04`),
together an unsigned 64-bit count of nanoseconds since the Unix epoch. Reads
are ordered low half first, because that access latches the high half; writes
are too, because the device commits the pair on the `TIME_HIGH` store.

The counter spans 1970-01-01 through 2554-07-21. An instant outside it is
refused with `DriverError::OutOfRange` rather than wrapped or clamped, and the
device's nanosecond precision means a `set` / `read` round trip preserves the
sub-second part exactly.

## Limitations

* **No alarm.** `ALARM_LOW` (`0x08`) through `CLEAR_INTERRUPT` (`0x1C`) are
  never touched: the driver neither arms nor services an alarm, and its one
  client polls the counter.
* **Nothing to report but the count.** The device models no backup cell and
  no oscillator-stopped flag, and the `virt` device tree declares neither, so
  `RtcStatus::battery_backed` is `false` and the counter is its own health
  signal: a reading of zero is the Unix epoch, which no running machine
  reports, so `read` answers `Ok(None)` and `oscillator_stopped` is set rather
  than handing on a value the device has nothing behind. Writing the epoch is
  honoured and then reads back the same way, because the device cannot tell it
  from an unprovisioned counter.
* **No plausibility judgement.** Whether a non-zero count is a believable
  wall time is clock policy and belongs to `userland/system/timed`, the one
  holder of `CAP_TIME_SET`.

## Capabilities

`register` requires `CAP_DRV_LOAD`. The `Run` binary is granted `CAP_MMIO_MAP`
(its register window, from its matched node's requested resources) and
`CAP_IPC_BIND_PRIVILEGED` (to create the restricted-sender endpoint). It holds
no clock authority: `RTC_ENDPOINT` admits only senders holding `CAP_TIME_SET`,
and the driver never tags a reading's provenance itself.

## Tests

`src/tests.rs` covers the load gate, the bind table, the two-half decode and
its round trip, sub-second and pre-1970 / post-2038 / end-of-range instants,
the zero-counter refusal, and a window too short for the counter pair.
`tairix-test-rtc-goldfish-qemu-riscv64` is the end-to-end witness: it boots
the production pipeline against a store carrying this bundle alone and
requires the applied `wall_secs` to land in the window the harness pinned the
emulated chip to.
