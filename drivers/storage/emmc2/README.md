# `tairix-drv-storage-emmc2` — Raspberry Pi 4 EMMC2 SD-host driver

`plans/PI.md` P8 deliverable. Implements `tairix_abi::driver::block::Block`
for the Raspberry Pi 4 (BCM2711) EMMC2 controller, an Arasan /
SDHCI-5.1 SD host.

## Transfer paths — DMA where possible

The fast path is **32-bit ADMA2 DMA**. The controller masters a whole
transfer chunk over the DAT lines through a one-entry ADMA2 descriptor
table the engine stages in a device-shared bounce region
(`DMA_STAGE_BLOCKS` = 128 blocks / 64 KiB per chunk, one descriptor), so a
multi-block transfer completes with a **single** transfer-complete
interrupt instead of a per-block buffer handshake, and the CPU never moves
data word-by-word through the slow uncached buffer data port. Larger
requests are split into successive 64 KiB chunks. The engine selects this
path at bring-up when the host grants a device-shared DMA staging region
(through `SdhciHost::dma_region`) that is at least `DMA_REGION_BYTES` long
and wholly 32-bit-addressable; enabling ADMA2 is a single `CONTROL0`
read-modify-write (`enable_adma2`).

The fallback path is **programmed I/O** (PIO): blocks move one 512-byte
block at a time through the SDHCI buffer data port (`CMD17`/`CMD18` reads,
`CMD24`/`CMD25` writes). It needs no DMA capability and runs whenever the
host grants no usable DMA region — DMA where possible, correct everywhere.
The command/transfer-mode encoding is shared between the two paths
(`read_command`/`write_command`) so the DMA-enable, direction, block-count,
and auto-CMD12 bits cannot drift.

The engine supports both coherent/Normal-Non-Cacheable staging memory and a
cacheable slab carrying `DmaSlab`'s coherency callback. Before each command
it synchronizes the active data range plus the ADMA descriptor and then
issues `dma_wmb` before the doorbell. After a read completion it issues
`dma_rmb`, synchronizes the device-written data range, and only then copies
the bytes out. The Raspberry Pi 4 bootstrap host uses the callback to run
aarch64 `dc civac` maintenance because EMMC2 does not snoop the CPU caches;
coherent hosts keep the callback as a no-op.

**Stability tier:** `experimental`. The DMA and PIO read/write paths are
both complete and host-tested, and the driver is wired into the aarch64
root-unlock bring-up over the DMA fast path (`plans/PI.md` B4 —
`crate::aarch64::root_unlock::emmc2_unlock`); metal acceptance on a real
Pi 4 is the remaining P8/B4 item (`raspi4b` cannot model EMMC2,
`plans/PI.md` §0.4).

## Layered seam

The SDHCI command/response and block-transfer state machine (`Emmc2`) is
written against the `SdhciHost` register seam, not a concrete memory
mapping:

- **Metal** drives it over `IrqSdhci`: a capability-gated `RegisterWindow`
  for `read32`/`write32` paired with a `CompletionWait` for `await_irq`,
  and — on the fast path — a `DmaSlab` for `dma_region`, all supplied by
  `wiring::open_discovered` from the discovered node. The kernel's
  `CompletionWait` re-arms and parks on the controller's GIC line, and the
  DMA slab is carved from a `CAP_MEM_DMA`-gated kernel DMA pool and carries
  the platform cache-maintenance callback (`Emmc2DmaHost`); the driver crate
  is generic over `lib/abi` only
  (`AGENTS.md` §3 / §17.4), so this inversion point keeps the kernel's
  IRQ-wait and DMA machinery out of the driver (mirrors the virtio host's
  `notify_wait`, `AGENTS.md` §2.2).
- **Host tests** drive it over `MockSdhci`, a register-level model of the
  controller plus a backing card, which models the ADMA2 engine (it walks
  the descriptor table the driver stages and moves the data), records every
  DMA synchronization range, and models the PIO buffer port; its `await_irq`
  is a no-op because the model surfaces completions inline.

This mirrors the `rpi_hvs` mailbox seam (`AGENTS.md` §2.2): the protocol
layer is proven host-side, the register block below it on metal. There is
no Pi-board QEMU vertical — QEMU does not model the EMMC2 controller
(`plans/PI.md` §0.4), so the emulation artefact is the host full-chain
test and metal acceptance is a documented checklist.

## SD identification

`Emmc2::open` runs the standard SD bring-up over the SDHCI register block:
controller reset → **power the card rail** (SD Bus Power on, 3.3 V via the
`CONTROL0` power-control byte) → identification clock → `CMD0` (idle) → `CMD8`
(interface condition, v2 check pattern) → `ACMD41` (operating conditions,
polled to power-up) → `CMD2`/`CMD3` (CID / RCA) → `CMD9` (CSD) → `CMD7`
(select) → `CMD16` (512-byte block length) → **`ACMD6` (4-bit bus)** →
**raise the SD clock to the data rate**. The block geometry is derived
from the card's CSD (structure v2 / high-capacity), never assumed
(`AGENTS.md` §18.5).

Identification runs at the SD identification clock (≤400 kHz) on the 1-bit
bus the controller resets to. Once the card is selected in the transfer
state, two pure speed steps run before any block transfer: `ACMD6` switches
the card to the 4-bit bus and the controller's `CONTROL0` data-width bit is
set to match (4×), and the SD clock is raised from the identification
divisor to the data divisor (`DATA_CLOCK_DIVISOR`, derived as
`IDENT_CLOCK_DIVISOR / 32` so the data clock is 32× the identification
clock — ≤12.8 MHz, within SD Default Speed's 25 MHz ceiling, so no
high-speed mode switch or tuning is needed). Together these turn the
~50 KB/s identification-clock 1-bit path into the ~6 MB/s Default-Speed
4-bit path (`AGENTS.md` §2.16); the divisor is derived from the
identification divisor rather than a base-clock constant so it carries no
board assumption of its own (`AGENTS.md` §2.20). The clock change follows
the SDHCI sequence: stop `SDCLK`, reprogram the frequency-select divisor,
wait for clock-stable, re-enable `SDCLK`.

The full host-controller reset clears SD Bus Power, and the standard
register block gates all command/data activity on it, so the power-on
write must precede the first command; without it `CMD0` never completes
(the bus is dark — the failure a real Pi 4 reported at
`stage=CMD0 GO_IDLE_STATE`). Linux's Pi 4 EMMC2 brings the same power
register up to `0x0F`.

Only high-capacity, block-addressed (SDHC/SDXC, CSD structure v2) cards
are supported; a byte-addressed standard-capacity card, a pre-v2 card, or
a structure-v1 CSD is rejected fail-closed with `DriverError::Unsupported`
rather than mis-addressed (`AGENTS.md` §5.4).

The CSD is decoded from the R2 response **exactly as the SDHCI controller
presents it**: the 8-bit CRC tail is stripped and the remaining 120 bits
are right-aligned across `RESP0..3`, so `CSD_STRUCTURE` (CSD[127:126]) sits
at `RESP3` bits [23:22] (the high byte of `RESP3` is zero padding above the
field) and `C_SIZE` (CSD v2) at `RESP1` bits [29:8]. Reading the structure
field at the wrong position made a real Pi 4's valid SDHC card decode as an
unsupported structure and fail at `stage=CMD9 SEND_CSD`; `MockSdhci` now
models the same right-aligned layout so the decode is proven against the
real register positions.

Because there is no Pi-board QEMU vertical, a failed bring-up on a real Pi
4 is otherwise blind. `Emmc2::open` therefore returns a `BringUpFault`
pairing the `DriverError` with a `BringUpStage` naming the exact step that
stalled (`MapWindow` / `ResetClock` / `GoIdle` / `SendIfCond` / `OpCond` /
`AllSendCid` / `SendRelativeAddr` / `SendCsd` / `SelectCard` /
`SetBlockLen` / `SetBusWidth` / `RaiseClock`). `BringUpStage::as_str` gives the stable operator-facing
name the in-kernel root-unlock path logs as a `stage=` field, alongside the
`DriverError` as an `error=` field that distinguishes a controller/command
fault from a decode rejection at the same step; a consumer that only needs
the §8 `DriverError` drops the stage with `?` / `DriverError::from`
(`AGENTS.md` §2.16 — measure, do not guess).

## Supported hardware

| Device                | Board   | Status                              |
|-----------------------|---------|-------------------------------------|
| `brcm,bcm2711-emmc2`  | Pi 4    | ADMA2 DMA + PIO read/write (host-tested); metal pending |

The aarch64 `FdtDiscovery` walk emits the `brcm,bcm2711-emmc2` node into
`tairix_abi::hwtree` (Storage class, translated MMIO window) from a
Pi-shaped device tree. The driver publishes a canonical `BIND_KEYS` table
(`AGENTS.md` §18.3) with one entry matching `compatible =
"brcm,bcm2711-emmc2"`; the discovered node is resolved against it and the
host calls `wiring::open_discovered` with the discovered register-window
base. As part of the storage **bootstrap floor** (`AGENTS.md` §18.6) — the
Pi 4's root volume must be readable before the signed driver store is
reachable — the driver is registered in the kernel binary's
`driver_catalog::IN_KERNEL_DRIVERS` floor registry against a build-signed
manifest carrying this same `BIND_KEYS`, and binds by discovery-match
through the same shared `lib/devmatch` policy the user-space `devmgr` uses
(§2.2).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered register window
  (`wiring::open_discovered`); the window is reached only through the
  host's `MmioMapper`, never a pointer the driver synthesises
  (`AGENTS.md` §4 — no ambient authority).
- `CAP_MEM_DMA` (fast path only) to carve the ADMA2 staging region through
  the host's `DmaHost`; the memory is reached only through that carve,
  never a synthesised address. Without it the DMA carve is refused and the
  driver runs on PIO rather than failing the bring-up (DMA where possible).

## Completion waits

The command-complete and transfer-complete waits (`wait_interrupt`) **park**
on the controller's interrupt through `SdhciHost::await_irq` between status
re-reads rather than busy-spinning the CPU (`AGENTS.md` §17.1 / §2.16). The
identification-only register polls that have no completion source — the
reset and clock-stable handshakes (`wait_clear`/`wait_set`) — still spin
briefly, but each is bounded by a poll budget (`DEFAULT_POLL_BUDGET`).
Every wait fails closed with `DriverError::DeviceFault` rather than waiting
forever (`AGENTS.md` §2.1): the budget bounds the spins, and it also caps
the number of completion parks as a fail-closed backstop against a storm of
spurious wake-ups. The budget is a defence bound, not a scalable capacity
(`AGENTS.md` §24.4).

`reset_and_clock` programs the controller's signal-enable register
(`IRPT_EN`) so it raises its CPU interrupt line on each completion (and on
every error bit); the kernel binds, routes, and arms that GIC line and
wakes the parked task (`crate::aarch64::root_unlock::emmc2_unlock`).

## Test surface

`cargo test -p tairix-drv-storage-emmc2` exercises:

- `CMDTM` command-word encoding and CSD-v2 capacity decode at the real
  right-aligned register positions, including that a structure value placed
  above the field is not mistaken for v2 (`command`).
- Full identification and reported geometry over `MockSdhci`.
- Bring-up leaves the card on the 4-bit bus at the data clock: `ACMD6`
  carries the 4-bit argument, the `CONTROL0` data-width bit is set, the
  `CONTROL1` frequency-select is reprogrammed to `DATA_CLOCK_DIVISOR` with
  `SDCLK` re-enabled, and the SD bus power the same `CONTROL0` register
  holds is preserved across the width read-modify-write
  (`bring_up_switches_to_the_4bit_bus_and_data_clock`); the data clock
  stays within Default Speed (`the_data_clock_stays_within_sd_default_speed`).
- Single-block and multi-block reads returning the card's data.
- Single-block and multi-block writes read back through the same mock
  card, with neighbouring blocks proven untouched.
- The ADMA2 fast path over the DMA-capable mock: bring-up selects ADMA2 in
  `CONTROL0` (preserving bus power and the 4-bit width) while a PIO-only
  host is never switched to ADMA2; single- and multi-block DMA reads and
  writes move the right data; a transfer larger than one 64 KiB chunk is
  split into successive DMA commands and reassembled in order, and a
  multi-chunk write/read DMA round trip is lossless; a DMA command error
  and an out-of-range LBA fail closed.
- 32-bit ADMA2 descriptor encoding (`Valid`/`End`/`Tran`, the 65536-as-0
  length convention) (`adma`).
- Range / shape validation (`BufferTooSmall`, `LengthOutOfRange`) on
  both the read and write paths.
- Byte-addressed, pre-v2, and CSD-v1 cards rejected `Unsupported`, each
  reporting the `BringUpStage` it failed at (`OpCond` / `SendIfCond` /
  `SendCsd`).
- Command-error (read and write) and stalled-controller fail-closed
  (`DeviceFault`; the stall is localised to the `GoIdle` stage).
- An unpowered SD bus (a rail that never comes up) fails closed at the
  `GoIdle` stage, proving the engine depends on the bus-power write.
- Every `BringUpStage` maps to a distinct, non-empty name and
  `BringUpFault` converts to its `DriverError`.
- A read parks on the completion interrupt until the controller signals,
  proving the engine never busy-spins the status register
  (`interrupt_driven_read_parks_until_the_controller_signals`).
- Bring-up programs the completion-signal enable so the controller raises
  its interrupt line (`reset_enables_the_completion_interrupt_signal`).
- The `wiring` capability / mapper gate (failing at the `MapWindow` stage).
- The `BIND_KEYS` table matches the `brcm,bcm2711-emmc2` node and rejects
  the sibling `brcm,bcm2711-pcie` node (`AGENTS.md` §18.3).

## Metal acceptance (pending hardware)

The on-metal bring-up checklist (boot a real Pi 4, observe the root-unlock
kthread mount the read-only `/System` volume and the encrypted root off the
SD card, and capture the UART log) is the acceptance artefact, recorded in
`plans/PI.md` P8/B4. It requires a physical Pi 4; no further code is staged
for it — the bring-up is wired (`crate::aarch64::root_unlock::emmc2_unlock`).
If the card does not come up, the `EventId(4139)` unlock-service error line
carries a `stage=` field naming the SD step that stalled and an `error=`
field naming how it failed, which localises the fix without re-flashing
blind.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Emmc2`
type is re-exported so the driver host can construct an instance through
`wiring::open_discovered`; the host never reaches into the type beyond the
`Block` trait surface. `BringUpStage` and `BringUpFault` are public only as
the error surface of `Emmc2::open` / `wiring::open_discovered`, so a metal
caller can log the failing stage; the §8 `register` contract is unchanged.

The DMA seam is public only where the metal host must implement or feed it:
`SdhciHost::dma_region` and its `DmaRegion` view (the host supplies the
device-shared staging bytes + device base), `IrqSdhci::with_dma` (pair the
window and waiter with a `DmaSlab`), and the `DMA_STAGE_BLOCKS` /
`DMA_REGION_BYTES` sizing constants the kernel wiring uses to size its DMA
carve. The `adma` module exposes the 32-bit descriptor encoder. None of
this widens the `register` contract.
