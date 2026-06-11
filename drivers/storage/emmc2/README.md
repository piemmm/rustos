# `rustos-drv-storage-emmc2` — Raspberry Pi 4 EMMC2 SD-host driver

`plans/PI.md` P8 deliverable. Implements `rustos_abi::driver::block::Block`
for the Raspberry Pi 4 (BCM2711) EMMC2 controller, an Arasan /
SDHCI-5.1 SD host. The transfer path is programmed-I/O (PIO): the card is
read one 512-byte block at a time through the SDHCI buffer data port, so
the read path needs no DMA capability.

**Stability tier:** `experimental`. The read path is complete; the write
path is the staged remainder of P8 (`Block::write_blocks` returns
`DriverError::Unsupported` until it lands).

## Layered seam

The SDHCI command/response and block-transfer state machine (`Emmc2`) is
written against the `SdhciHost` register seam, not a concrete memory
mapping:

- **Metal** drives it over a capability-gated `RegisterWindow`
  (`SdhciHost` is implemented for it), mapped from the discovered node by
  `wiring::open_discovered`.
- **Host tests** drive it over `MockSdhci`, a register-level model of the
  controller plus a small backing card.

This mirrors the `rpi_hvs` mailbox seam (`AGENTS.md` §2.2): the protocol
layer is proven host-side, the register block below it on metal. There is
no Pi-board QEMU vertical — QEMU does not model the EMMC2 controller
(`plans/PI.md` §0.4), so the emulation artefact is the host full-chain
test and metal acceptance is a documented checklist.

## SD identification

`Emmc2::open` runs the standard SD bring-up over the SDHCI register block:
controller reset → identification clock → `CMD0` (idle) → `CMD8`
(interface condition, v2 check pattern) → `ACMD41` (operating conditions,
polled to power-up) → `CMD2`/`CMD3` (CID / RCA) → `CMD9` (CSD) → `CMD7`
(select) → `CMD16` (512-byte block length). The block geometry is derived
from the card's CSD (structure v2 / high-capacity), never assumed
(`AGENTS.md` §18.5).

Only high-capacity, block-addressed (SDHC/SDXC, CSD structure v2) cards
are supported; a byte-addressed standard-capacity card, a pre-v2 card, or
a structure-v1 CSD is rejected fail-closed with `DriverError::Unsupported`
rather than mis-addressed (`AGENTS.md` §5.4).

## Supported hardware

| Device                | Board   | Status                              |
|-----------------------|---------|-------------------------------------|
| `brcm,bcm2711-emmc2`  | Pi 4    | read path (host-tested); metal pending |

The aarch64 `FdtDiscovery` walk emits the `brcm,bcm2711-emmc2` node into
`rustos_abi::hwtree` (Storage class, translated MMIO window) from a
Pi-shaped device tree; `devmgr` matches the node against this driver's
bind table (`AGENTS.md` §18.3) and the host calls `wiring::open_discovered`
with the discovered register-window base.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered register window
  (`wiring::open_discovered`); the window is reached only through the
  host's `MmioMapper`, never a pointer the driver synthesises
  (`AGENTS.md` §4 — no ambient authority).

## Bounded waits

Every controller wait (reset, clock-stable, command-complete,
buffer-ready, transfer-complete, `ACMD41` power-up) is bounded by a poll
budget (`DEFAULT_POLL_BUDGET`). Exceeding it fails closed with
`DriverError::DeviceFault` rather than spinning forever (`AGENTS.md`
§2.1); the budget is a defence bound, not a scalable capacity
(`AGENTS.md` §24.4).

## Test surface

`cargo test -p rustos-drv-storage-emmc2` exercises:

- `CMDTM` command-word encoding and CSD-v2 capacity decode (`command`).
- Full identification and reported geometry over `MockSdhci`.
- Single-block and multi-block reads returning the card's data.
- Range / shape validation (`BufferTooSmall`, `LengthOutOfRange`).
- Byte-addressed, pre-v2, and CSD-v1 cards rejected `Unsupported`.
- Command-error and stalled-controller fail-closed (`DeviceFault`).
- Staged read-only write (`Unsupported`).
- The `wiring` capability / mapper gate.

## Metal acceptance (pending hardware)

The on-metal bring-up checklist (read the FAT boot partition and the
RustFS root from a real card, capture the UART log) is the acceptance
artefact, recorded in `plans/PI.md` P8. It requires a physical Pi 4; no
further code is staged for it.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Emmc2`
type is re-exported so the driver host can construct an instance through
`wiring::open_discovered`; the host never reaches into the type beyond the
`Block` trait surface.
