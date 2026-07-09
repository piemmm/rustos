# `rustos-drv-storage-usb-msd` — USB mass-storage class driver

`plans/DEVICES.md` D2. The autoloaded **user-space USB mass-storage *class*
driver** — the `Run` binary `devmgr` spawns when a bulk-only mass-storage
**interface** node (class `08:06:50` — SCSI transparent command set over
Bulk-Only Transport) is discovered (`AGENTS.md` §18). The crate is a `lib`
(its `BIND_KEYS` bind table plus the host-testable device logic:
configuration-descriptor reader, BOT/SCSI engine, block-service state
machine) **and** a `Run` binary (the process).

It is a pure class driver: it touches **no** controller register, owns **no**
controller DMA, holds **no** IRQ line. The host-controller driver
(`drivers/bus/usb/xhci`) owns the controller and serves this interface's
control and bulk transfers over the bus-agnostic URB transport. The same
binary works behind any host controller that speaks the URB transport
(`AGENTS.md` §2.20 / §17.4).

## What it does

`main` (a freestanding pure-Rust `rustos-rt` program):

1. Builds `rustos_drvrt::RtDriverHost::from_grants_query` over its granted
   resources (the interface node's two transport grants — no MMIO/DMA), reads
   the URB endpoint id and maps the shared URB data buffer.
2. Reads the device's own **configuration descriptor** over control-IN and
   derives — never assumes — the mass-storage interface number and its first
   bulk-IN + bulk-OUT endpoint pair (`src/desc.rs`, fail-closed on every
   hostile length).
3. Runs `GET MAX LUN`, then per LUN: `INQUIRY` (non-disk units are skipped),
   a **bounded** `TEST UNIT READY`/`REQUEST SENSE` ready drain,
   `READ CAPACITY(10)`/`(16)` (validated geometry; 16-byte form past the
   32-bit LBA horizon), and the `MODE SENSE(6)` write-protect bit.
4. Publishes one **storage-class** hardware-tree node per ready LUN, carrying
   a `rustos,usb-msd-lun` compatible key and two grants: a block-service call
   endpoint and a 32 KiB shared data window (`rustos_abi::blkio` — the
   request/completion frames a consumer such as the D3 volume manager
   drives).
5. Parks on a kernel wait-set over the LUN endpoints and serves each
   `BlkRequest` (geometry / read / write / flush) through the BOT engine:
   CBW/CSW framing with every device field validated, per-transfer stall
   recovery below (the HCD), CSW-stall retry, and Bulk-Only Mass Storage
   Reset on tag mismatch / corrupt CSW / phase error (`src/bot.rs`).
   Write-protected media refuse writes driver-side before any byte reaches
   the device. A vanished interface retracts every LUN node and exits `0` so
   `devmgr` reloads the driver on re-plug.

Failures exit with reserved fail-closed codes (`80` no host, `81` no
transport, `82` bring-up refused every unit, `83` block service could not be
stood up), leaving the volume absent rather than wedged (`AGENTS.md` §2.9).

## Least privilege (`AGENTS.md` §5.4)

`CAP_SHM` (map the granted URB buffer; create the per-LUN data windows),
`CAP_IPC_ENDPOINT` (submit URBs on its one interface's transport endpoint),
`CAP_IPC_BIND_PRIVILEGED` (bind the per-LUN block-service endpoints),
`CAP_HW_EMIT` (publish/retract the per-LUN storage nodes), `CAP_LOG_EMIT`
(diagnostics). A compromised disk driver cannot reprogram the controller,
reach another device's buffer, or touch the bus.

## Supported hardware / limitations

Any USB mass-storage device exposing a SCSI-transparent **bulk-only**
interface (`08:06:50`) a host-controller driver enumerates and serves over
the URB transport (the Pi 4's USB-A ports via the xHCI HCD) — the shape of
essentially every USB flash stick and USB disk enclosure. Up to 16 LUNs.
Logical block sizes 512–4096 (powers of two). Not covered (deliberately, per
`plans/DEVICES.md` §3): UAS and CBI transports, non-disk SCSI types (tape,
optical). Runtime unload is driven by `devmgr` through the kernel
driver-unload mechanism; the driver also exits by itself when its interface
vanishes.

## Tests

The device logic is host-proven in this crate over scripted doubles:
`src/bot_tests.rs` (CBW/CSW framing, tag mismatch and corrupt-CSW reset
recovery, CSW-stall retry, stalled data phases, short reads, sense mapping,
capacity validation including a 100 TB-class unit, write-protect enforcement,
chunking, multi-LUN), `src/desc.rs` (hostile descriptor streams), and
`src/serve.rs` (the block-service request surface over an in-memory device).
The URB transport is proven in `lib/usb` (including the D1 bulk and the D2
no-data control-OUT seams). The end-to-end path is the metal acceptance item
(QEMU models no Pi USB; `plans/PI.md` §0.4).
