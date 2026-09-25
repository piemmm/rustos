# tairix-virtio

The bus-agnostic virtio 1.x protocol (`lib/virtio`): the one implementation
every virtio driver and the kernel-side virtio hosts share, so no driver
carries its own queue code.

## API

- `Transport` — the device seam (status, features, queue programming, notify,
  configuration space), implemented here for MMIO (`MmioTransport`) and PCI
  (`PciTransport`); `MockTransport` is the in-process device the driver tests
  run against, shared with the host playing it through `into_shared`. The
  mocks are built only with the `mock` feature, which consumers enable in
  `[dev-dependencies]` alone, so no production build carries them.
- `SplitQueue` — the split virtqueue. Its free list and chain links live in
  driver memory, and a completion is accepted only for the head of a chain the
  device holds, so a device writing over the descriptor table or naming a
  chain it was never given cannot corrupt the queue. Descriptors are reissued
  oldest-returned first, so a repeated completion names free ones. Opening
  one names the most descriptors the driver keeps on it at once, and a device
  that cannot hold them is refused (`QueueTooShallow`) before it is given a
  ring.
- `RequestQueue` — one request at a time over a `SplitQueue`, bounded by a
  budget measured on the host's clock and against a wake storm by
  `MAX_COMPLETION_WAKES`, ended by a wait that times out or cannot be made,
  and never published over a completion already in the ring. A request the
  device leaves unanswered keeps its chain and buffers the device's until it
  hands them back; nothing is published before then, and the device is
  reminded of the chain at most once per the request's budget.
- `PackedQueue` — the packed virtqueue, sized as `SplitQueue` is.
- `DmaSlab`, `BounceBuffer`, `scrub`, `VirtioHost`, `MockHost` — owned
  device-visible memory, the zero-on-free staging wrapper, the one zeroing a
  sensitive payload's staging gets once the device hands it back, the DMA
  allocation and wait seam, and its test implementation, whose waits play
  scripted `MockWait`s on a clock of their own and which reports whether each
  slab came back zeroed (`released_zeroed`). Memory a device may still
  master is withheld (`DmaSlab::withhold`, `SplitQueue::withhold`), neither
  freed nor scrubbed.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_op_in_unsafe_fn)]`; every `unsafe`
  block carries its `// SAFETY:` invariant.
- Every device-written field — a used-ring id, a written length — is
  untrusted and validated before it is acted on.
- The crate holds no capability of its own: DMA memory comes from the host the
  consuming driver was given.

## Stability

Tier: `experimental`.
