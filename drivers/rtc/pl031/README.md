# `tairix-drv-rtc-pl031`

The ARM PrimeCell PL031 real-time-clock driver: the `arm,pl031` counter the
QEMU aarch64 `virt` board exposes, and the `Firmware` rung of the wall clock's
provenance ladder on that target (`plans/TIMESYNC.md` TS-3).

Two targets, one crate. `src/lib.rs` is the device logic — the driver identity
(`register` + bind table) and the `Rtc` class implementation over a granted
`RegisterWindow` — host-tested against a mock register block. `src/main.rs` is
the `Run` binary `devmgr` autoloads into user space: it maps its granted
window, binds `RTC_ENDPOINT`, and parks in `call_recv` serving the `lib/abi`
RTC wire contract.

## Supported hardware

Any `arm,pl031` node whose register window the kernel granted the process. The
PrimeCell identification registers sit at the top of a 4 KiB page, so a granted
window is a page; the driver touches only `RTCDR` (`0x00`, the counter),
`RTCLR` (`0x08`, the load register), and `RTCCR` (`0x0C`, the control
register). The counter is an unsigned 32-bit count of seconds since the Unix
epoch, so it spans 1970-01-01 through 2106-02-07; an instant outside it is
refused rather than wrapped or clamped.

Bring-up sets `RTCCR`'s write-once start bit. A counter that will not start
vouches for nothing rather than offering a frozen register.

## Limitations

* **No alarm.** `RTCMR` and the interrupt registers are never touched: the
  driver neither arms nor services an alarm, and its one client polls.
* **One-second precision.** The part counts whole seconds, so a `set` / `read`
  round trip loses the sub-second part. The declared precision says so.
* **No backup indicator.** The part exposes none and the device tree declares
  none, so `RtcStatus::battery_backed` is `false` — an honest "cannot say"
  rather than a claim the hardware does not support.
* **No plausibility judgement.** Whether a reading is a believable wall time
  is clock policy and belongs to `userland/system/timed`, the one holder of
  `CAP_TIME_SET`.

## Capabilities

`register` requires `CAP_DRV_LOAD`. The `Run` binary is granted `CAP_MMIO_MAP`
(its register window, from its matched node's requested resources) and
`CAP_IPC_BIND_PRIVILEGED` (to create the restricted-sender endpoint). It holds
no clock authority: `RTC_ENDPOINT` admits only senders holding `CAP_TIME_SET`,
and the driver never tags a reading's provenance itself.

## Tests

`src/tests.rs` covers the load gate, the bind table, the counter decode and
its round trip, a counter that will not start, and the pre-1970 / post-2038 /
end-of-range instants. `tairix-test-rtc-pl031-qemu-aarch64` is the end-to-end
witness: it boots the production pipeline against a store carrying this bundle
alone and requires the applied `wall_secs` to land in the window the harness
pinned the emulated chip to.
