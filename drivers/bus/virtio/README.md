# `tairix-drv-bus-virtio` — cross-arch virtio transport

Stage 4 deliverable: the shared **virtio 1.x split-virtqueue** plumbing
that both `drivers/storage/virtio_blk` and `drivers/network/virtio_net`
build on. Per `AGENTS.md` §2.2 ("If two crates need it, it goes there")
the queue protocol lives once, here, and the per-device crates carry
only the device-specific wire format.

## What it is

- A `Transport` trait that abstracts over PCI (x86_64) and MMIO
  (aarch64 / riscv64 `virt`) bus seams.
- A `SplitQueue` implementation of the virtio 1.1 §2.6 split virtqueue
  (descriptor table, avail ring, used ring, free-descriptor pool).
- A `VirtioHost` trait through which a driver requests DMA-backed
  bounce buffers and parks pending completion.
- A `MockTransport` + `ChainView` test seam so block and network
  driver tests can drive descriptor chains end-to-end in-process.
- A `BounceBuffer` wrapper that honours `BufferClass::Sensitive` by
  zeroing its staging on drop (`AGENTS.md` §4).

## What it is **not** (deferred)

- **Packed queues** (virtio 1.1 §2.7) — split queues are sufficient
  for Stage 4. Packed support is a Stage 5 follow-up tracked in
  `docs/src/drivers/virtio.md`.
- **Real DMA from a per-process heap with a separate physical mapping.**
  The `MockHost` allocator returns `phys == virt` because the kernel
  per-process-heap-with-`phys_of()` API does not exist yet
  (`plans/WIRING.md`, item 1).
- **IRQ delivery into userland.** The current `notify_wait` is a
  polled cooperative hook; real interrupt routing is item 2 of the
  next-session prompt.
- **Live register access.** The `PciBackend` / `MmioBackend` types
  now own a capability-checked `RegisterWindow` minted by the
  kernel's MMIO-map facility (Stage 4.D Item 3); the remaining work
  is the concrete `Transport` impl that decodes the virtio register
  layout on top of that window.

## Supported hardware

The transport itself is hardware-agnostic. The backends shipped in
this crate are:

| Backend         | Bus  | Architectures            | Stage 4 status     |
|-----------------|------|--------------------------|--------------------|
| `PciBackend`    | PCI  | x86_64                   | register window    |
| `MmioBackend`   | MMIO | aarch64, riscv64         | register window    |

"Register window" means each backend owns a capability-checked
`RegisterWindow` (Stage 4.D Item 3) and exposes bounds-checked
register accessors over it; the wire protocol (status byte, feature
negotiation, queue programming, notification) is still exercised
through the `MockTransport` seam, and the concrete `Transport` impl
that decodes the virtio register layout on top of the window is the
remaining Stage 4.D work.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time. The transport crate itself is
  loaded as a user-space driver; it never asserts `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p tairix-drv-bus-virtio` exercises 30 unit tests
covering:

- Transport status progression (reset → ACK → DRIVER → FEATURES_OK
  → DRIVER_OK).
- `SplitQueue` free-list initialisation, descriptor chaining,
  free-pool exhaustion, used-ring wrap.
- `BounceBuffer` zero-on-free for `BufferClass::Sensitive`.
- `MockTransport` chain dispatch + `ChainView` round-trip.
- `VirtioError` ⇒ `DriverError` mapping.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
public *types* (`Transport`, `SplitQueue`, `BounceBuffer`, …) are
re-exported solely so that the two driver crates that depend on
this crate can use them without duplicating queue code.
