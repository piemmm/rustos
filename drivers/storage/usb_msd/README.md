# `rustos-drv-storage-usb-msd` — USB mass-storage class driver

`plans/DEVICES.md` D2/D5. The autoloaded **user-space USB mass-storage
*class* driver** — the `Run` binary `devmgr` spawns when a mass-storage
**interface** node it serves is discovered (`AGENTS.md` §18): SCSI
transparent over Bulk-Only Transport (`08:06:50`, the ubiquitous stick/disk),
UFI floppies over BOT (`08:04:50`) and over Control/Bulk/Interrupt
(`08:04:00`, the classic USB floppy drive), and USB Attached SCSI
(`08:06:62`). The crate is a `lib` (its `BIND_KEYS` bind table plus the
host-testable device logic: the configuration-descriptor reader, the
transport-neutral SCSI command layer, the three wire transports, and the
block-service state machine) **and** a `Run` binary (the process).

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
   derives — never assumes — the mass-storage interface number, its wire
   transport (the interface protocol byte), its command set (the sub-class
   byte), and that transport's endpoints: the bulk pair for BOT, bulk pair +
   completion interrupt for CBI, or the four Pipe-Usage-named bulk pipes for
   UAS (`src/desc.rs`, fail-closed on every hostile length).
3. Discovers the units (`GET MAX LUN` for BOT, `REPORT LUNS` for UAS,
   exactly one for CBI), then per LUN: `INQUIRY` (non-disk units are
   skipped), a **bounded** `TEST UNIT READY` ready drain (sense consumed
   per failed attempt — UAS delivers it in-band, BOT/CBI via
   `REQUEST SENSE`), `READ CAPACITY(10)`/`(16)` (validated geometry;
   16-byte form past the 32-bit LBA horizon), and the write-protect bit
   (`MODE SENSE(6)` for the transparent set, `MODE SENSE(10)` for UFI).
4. Publishes one **storage-class** hardware-tree node per ready LUN, carrying
   a `rustos,usb-msd-lun` compatible key and two grants: a block-service call
   endpoint and a 32 KiB shared data window (`rustos_abi::blkio` — the
   request/completion frames a consumer such as the D3 volume manager
   drives).
5. Parks on a kernel wait-set over the LUN endpoints and serves each
   `BlkRequest` (geometry / read / write / flush) through the shared SCSI
   command layer (`src/scsi.rs`) on the device's wire transport:
   - **BOT** (`src/bot.rs`): CBW/CSW framing with every device field
     validated, per-transfer stall recovery below (the HCD), CSW-stall
     retry, and Bulk-Only Mass Storage Reset on tag mismatch / corrupt CSW /
     phase error.
   - **CBI** (`src/cbi.rs`): the 12-byte command block over the ADSC
     control-OUT channel (a control STALL is the "command not accepted"
     answer), the bulk data phase, the two-byte completion interrupt (UFI
     ASC/ASCQ or the typed status spelling), and the Command Block Reset on
     a malformed or out-of-step completion. UFI floppies have no
     `SYNCHRONIZE CACHE`; a flush succeeds without a wire round trip.
   - **UAS** (`src/uas.rs`): tag-checked Command/Read-Ready/Write-Ready/
     Sense IU sequencing over the four pipes (USB 2.0 non-stream
     operation), with autosense delivered in-band and every hostile IU
     refused closed.
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

Any USB mass-storage device exposing an interface in the served set —
SCSI-transparent bulk-only (`08:06:50`, essentially every USB flash stick
and disk enclosure), UFI floppy drives over BOT (`08:04:50`) or CBI
(`08:04:00`), and USB Attached SCSI (`08:06:62`) — that a host-controller
driver enumerates and serves over the URB transport (the Pi 4's USB-A ports
via the xHCI HCD). Up to 16 LUNs. Logical block sizes 512–4096 (powers of
two). Deliberately not covered (per `plans/DEVICES.md` §3/D5): the
interrupt-less CB variant (protocol `0x01`), ATAPI sub-classes (tape,
optical), UAS command queueing / task-management IUs / SuperSpeed bulk
streams (one command in flight serves the synchronous block service; a
protocol violation fails the exchange closed), and the BOT→UAS alternate-
setting switch (a dual BOT+UAS device runs its default BOT setting; UAS
serves devices exposing it as the default interface). A medium inserted
after bring-up is picked up on re-plug, not polled for. Runtime unload is
driven by `devmgr` through the kernel driver-unload mechanism; the driver
also exits by itself when its interface vanishes.

## Tests

The device logic is host-proven in this crate over scripted doubles:
`src/scsi_tests.rs` (per-set CDB spelling, MODE SENSE forms, flush
semantics, the bounded ready drain, autosense vs `REQUEST SENSE`, LunBlock
validation/chunking/scrub), `src/bot_tests.rs` (CBW/CSW framing, tag
mismatch and corrupt-CSW reset recovery, CSW-stall retry, stalled data
phases, short reads, sense mapping, capacity validation including a
100 TB-class unit, multi-LUN), `src/cbi_tests.rs` (ADSC framing, the
stall-as-refusal answer, both completion spellings, Command Block Reset,
the floppy geometry), `src/uas_tests.rs` (IU sequencing, tag checking,
autosense in both sense formats, hostile IUs, `REPORT LUNS`),
`src/desc_tests.rs` (transport classification and hostile descriptor
streams, including UAS Pipe Usage validation), and `src/serve.rs` (the
block-service request surface over an in-memory device). The URB transport
is proven in `lib/usb` (bulk, the no-data and data-stage control-OUT, the
EP0 stall recovery, and the per-pipe bulk routing). The end-to-end path is
the metal acceptance item (QEMU models no Pi USB; `plans/PI.md` §0.4).
