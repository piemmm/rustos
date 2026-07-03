# `rustos-drv-storage-virtio-blk` — virtio-blk block driver

Stage 4 deliverable. Implements `rustos_abi::driver::block::Block` on
top of the cross-arch virtio transport in `drivers/bus/virtio`. The
driver is **bus-agnostic**: the same source compiles against the PCI
backend (x86_64) and the MMIO backend (aarch64, riscv64).

## Wire protocol

virtio 1.1 §5.2 — Stage 4 implements the unextended subset:

- One request queue (`requestq`, queue index 0).
- 16-byte `struct virtio_blk_req` header (type + reserved + sector).
- Fixed 512-byte logical sector size. `VIRTIO_BLK_F_BLK_SIZE` and
  `VIRTIO_BLK_F_TOPOLOGY` negotiation is a Stage 5 follow-up.
- Status byte: `VIRTIO_BLK_S_OK` (0), `VIRTIO_BLK_S_IOERR` (1),
  `VIRTIO_BLK_S_UNSUPP` (2). The first maps to `Ok(())`, the second
  to `DriverError::DeviceFault`, the third to
  `DriverError::Unsupported`.

## DMA staging

The header, data, and status DMA buffers are carved **once at `open`**
and reused for every request, so the read/write data path never
re-enters the per-process DMA allocator (and never emits a
per-request DMA-grant audit record). The data buffer is a fixed
staging window (`wire::MAX_TRANSFER_LEN`, 32 KiB); a `read_blocks` /
`write_blocks` call larger than the window is split into block-aligned
chunks that reuse the same buffers, so the driver's DMA footprint is a
fixed per-device cost rather than a function of the request length or
the volume size (`AGENTS.md` §2.16, §24, §26).

## Supported hardware

| Bus      | Architectures            | Stage 4 status                     |
|----------|--------------------------|-------------------------------------|
| virtio   | x86_64 / aarch64 / riscv64 | mock-transport only (see notes)   |

The "mock-transport only" status reflects the prerequisites the
`drivers/bus/virtio` README enumerates (per-process DMA mapping, IRQ
routing, bus-handle hand-off). Once those land the same `VirtioBlk`
type binds to a `PciBackend` / `MmioBackend` without modification.

## Discovery (bind table)

The driver publishes a canonical `BIND_KEYS` table (`AGENTS.md` §18.3)
with one entry matching the virtio block device id
(`HwMatchKey::virtio(2)`). A discovered virtio node whose probed device
id is `virtio-blk` binds this driver by match — never a kernel guess
(§18.5) — and the key carries no transport detail, so the same driver
binds the device over either the PCI or the MMIO transport. As part of
the storage **bootstrap floor** (`AGENTS.md` §18.6), the driver is
registered in the kernel binary's `driver_catalog::IN_KERNEL_DRIVERS`
floor registry against a build-signed manifest carrying this same
`BIND_KEYS`.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The class-trait methods (`Block::read_blocks`, `Block::write_blocks`,
  and their `_with_class` variants) are gated by ownership of the
  `DriverHandle` returned from `register`, as documented in
  `lib/abi/src/driver/block.rs`.

## Zero-on-free

`Block::read_blocks_with_class(_, _, BufferClass::Sensitive)` and the
corresponding `write_blocks_with_class` route every internal staging
copy through `BounceBuffer`, whose `Drop` impl scrubs the DMA region
before release (`AGENTS.md` §4). The caller-owned `buf` is **not**
zeroed; that scrubbing remains the caller's responsibility.

## Test surface

`cargo test -p rustos-drv-storage-virtio-blk` exercises:

- `open` reads geometry from the device-configuration window.
- Read / write / write-then-read round-trips against a sector-array
  fixture.
- Range validation (`BufferTooSmall`, `LengthOutOfRange`).
- Multi-sector read concatenation.
- Steady-state I/O allocates no new DMA (staging is reused).
- A transfer larger than the staging window chunks and round-trips.
- `BufferClass::Sensitive` round-trip.
- `register` capability gate.
- The `BIND_KEYS` table matches a `virtio-blk` node and rejects a
  different virtio device / a non-virtio node (`AGENTS.md` §18.3).

All host-side tests pass; per-arch QEMU integration is tracked under
item 4 of `.junie/next-session-prompt.md` (it depends on the kernel
DMA + IRQ work in items 1–2).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`VirtioBlk` type is re-exported so the driver host can construct an
instance; the host never reaches into the type beyond the `Block`
trait surface.
