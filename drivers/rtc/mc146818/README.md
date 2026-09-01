# `tairix-drv-rtc-mc146818`

The Motorola MC146818-compatible PC CMOS real-time-clock driver: the chip
every PC-compatible machine carries at I/O ports `0x70`/`0x71`, and the
`Firmware` rung of the wall clock's provenance ladder on x86_64
(`plans/TIMESYNC.md` TS-3).

Two targets, one crate. `src/lib.rs` is the device logic — the driver identity
(`register` + bind table) and the `Rtc` class implementation over the
`CmosPorts` access seam — host-tested against a model of the index/data pair
and the register file. `src/main.rs` is the `Run` binary `devmgr` autoloads
into user space: it resolves its granted port range, binds `RTC_ENDPOINT`, and
parks in `call_recv` serving the `lib/abi` RTC wire contract.

## Supported hardware

Any `motorola,mc146818` node whose port range the kernel granted the process.
On x86_64 that node is synthesised by the architecture port's legacy-fallback
discovery path: no ACPI table enumerates the chip, and every PC-compatible
machine has it at the same fixed pair, which is exactly why it is a *fallback*
rather than a discovered node. The driver still binds by matching that node,
so the assumption never reaches the driver.

The chip is a byte-addressed calendar register block reached indirectly: a
write to the index port selects a register, and the data port then reads or
writes its byte. The driver touches seconds (`0x00`), minutes (`0x02`), hours
(`0x04`), day of month (`0x07`), month (`0x08`), year (`0x09`), and the three
status registers A (`0x0A`), B (`0x0B`), and D (`0x0D`).

Register B's format bits are honoured, never imposed: bit 2 (`DM`) selects
plain binary over packed BCD and bit 1 selects 24-hour over 12-hour, and a
`set` writes the fields back in whatever format it finds. In 12-hour mode bit
7 of the hours register is the PM flag and is masked off before conversion,
with 12 AM reading as hour 0 and 12 PM as hour 12.

Reads ride out the chip's update window. Status A bit 7 is
update-in-progress, and a read taken while it is set can straddle a carry, so
the driver probes the bit clear, reads the whole block, reads it again, and
accepts only two blocks that agree. Writes raise Register B's `SET` bit across
the field stores and lower it afterwards — including on the error path, so a
chip is never left frozen — so the chip cannot advance mid-write.

## Limitations

* **No century register.** `0x32` holds a century on most modern chipsets, but
  *whether* it does is declared by the ACPI FADT, which this driver never
  sees, so it is not read. The two-digit year is resolved through the shared
  `resolve_two_digit_year` plausibility window instead, which spans one
  hundred years from the release epoch — a year that window admits is one the
  wall clock would have accepted anyway. `set` refuses an instant whose year
  the window would resolve to a different century rather than storing digits
  the chip could only report back as another year.
* **A stuck update window has no time.** If Status A never reports the window
  clear, or the block never reads the same twice, `read` answers `Ok(None)`
  within a fixed budget rather than spinning — the sanctioned bounded hardware
  handshake, not a wait for work. The two budgets are separate because they
  defend against different facts and cost differently: the update-window poll
  is sized to the chip's ~2 ms window, while the agreement retry needs only a
  handful of attempts (a tick comes once a second and a whole-block read takes
  tens of microseconds) and each attempt costs six times a window probe. A
  window that never closes is terminal and is not retried on top of that.
* **The health flag is not clearable.** Register D's valid-RAM bit (`VRT`) is
  read-only and reflects the backup cell, so a flat cell makes `read` answer
  `Ok(None)` with `oscillator_stopped` set, and a `set` cannot make the chip
  vouch again. That is a board condition, not something software can fix.
* **No alarm.** The alarm registers (`0x01`, `0x03`, `0x05`) and Register C's
  interrupt flags are never touched: the driver neither arms nor services an
  alarm, and its one client polls.
* **The index port's NMI-mask bit is not preserved.** On PC hardware bit 7 of
  `0x70` masks NMI, and every register this driver selects has that bit clear,
  so a select leaves NMI unmasked. The port is write-only, so its current
  state cannot be read back and preserved from a two-port grant; Linux's own
  CMOS accessors behave the same way. NMI masking is a platform concern the
  kernel owns, not this driver's.
* **No plausibility judgement.** Whether a decoded date is a believable wall
  time is clock policy and belongs to `userland/system/timed`, the one holder
  of `CAP_TIME_SET`.

## Capabilities

`register` requires `CAP_DRV_LOAD`. The `Run` binary is granted
`CAP_MMIO_MAP` — the capability a `Port` device resource has always required,
since reaching a device's registers is one authority whether they are
addressed as memory or as ports, so no new capability is defined — and
`CAP_IPC_BIND_PRIVILEGED` (to create the restricted-sender endpoint). Every
transfer goes through the capability-gated `port_read` / `port_write` traps,
which re-check the capability and re-bound `port .. port + width` inside the
granted range kernel-side, so the driver can reach nothing its matched node
did not request.

It holds no clock authority: `RTC_ENDPOINT` admits only senders holding
`CAP_TIME_SET`, and the driver never tags a reading's provenance itself.

## Tests

`src/tests.rs` covers the load gate, the bind table, BCD and binary decode,
24-hour and 12-hour hours (including 12 AM and 12 PM, and every hour of the
day in both encodings), an hour or field the chip cannot legally hold, an
update window that settles and one that never does, a torn read rejected by
the double read, an update falling between two blocks discarding the earlier
one, a flat backup cell, the century register being ignored, the
two-digit-year window in both directions, `set` round-tripping in all four
formats with `SET` raised and lowered, a mid-write refusal still releasing the
chip, a year outside the window refused whole, and pre-1970 / post-2038
instants. The end-to-end autoload path — node synthesis, the signed-bundle
autoload, the granted port reads, and `timed` applying the reading — is
proven by `tests/integration/rtc_mc146818_qemu_x86_64`, which gates on the
applied second landing in the window the harness pinned the emulated chip to.
