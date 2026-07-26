# `tairix-virtio-net` — virtio-net device engine

Stability tier: **experimental**.

Arch-neutral, transport-agnostic virtio-net link-layer device logic: the
bring-up (virtio 1.1 §3.1 init sequence, `VIRTIO_NET_F_MAC`) and the
frame-ring `tairix_abi::driver::net::Net` service (reap completed
transmissions, drain the TX ring into the device, harvest delivered
frames into the RX ring), written once over
the bus-agnostic `lib/virtio` transport so the same source compiles
against the PCI (x86_64) and MMIO (aarch64, riscv64) backends.

This is a `lib/*` device-logic crate, the `lib/virtio_input` precedent.
Living in `lib/*` — rather than in the `drivers/network/virtio_net`
crate — is what lets a user-space driver *process* link the engine
directly: a process crate may depend on `lib/*` but never on another
`drivers/*` crate (`AGENTS.md` §17.4). The driver-host registration shell
(`register`) that wraps this engine is the `tairix-drv-network-virtio-net`
crate, which re-exports `VirtioNet` from here.

- **The service doorbell never waits on the device.** It moves what is
  ready and returns, so it is safe to serve across the live process
  boundary (the driver process answering the stack's `Service` request),
  where parking would block the reply and the serve loop. A transmit
  frame is handed to the device and left in flight; its completion is
  reaped non-blockingly on a later call (the shared device interrupt the
  completion raises drives it). With one staging pair a further queued
  frame is held in the ring — back-pressure, never a wait or a drop.
- **Negotiated offloads** (each only when the device offers it, so the
  software path is always the fallback): receive-checksum validation
  (`VIRTIO_NET_F_GUEST_CSUM`), transmit TCP-checksum
  (`VIRTIO_NET_F_CSUM`), TCP segmentation (`HOST_TSO4`+`TSO6`), and
  **mergeable receive buffers** (`VIRTIO_NET_F_MRG_RXBUF`). With
  mergeable buffers the header is the 12-byte `virtio_net_hdr_mrg_rxbuf`
  on both rings, the driver posts a *pool* of receive buffers (so a burst
  is captured before the stack next services the ring, not dropped past a
  single outstanding buffer), and it reassembles a frame the device split
  across several buffers — bounded to one link frame, fail-closed on an
  over-long or corrupt `num_buffers`.
- **Multiqueue receive** (`VIRTIO_NET_F_MQ` + `VIRTIO_NET_F_CTRL_VQ`):
  when both are offered and the device advertises more than one pair, the
  driver reads `max_virtqueue_pairs`, brings up one receive + one transmit
  virtqueue per enabled pair (bounded by `RingGeometry::MAX_RX_QUEUES`),
  and selects the pair count through the control-queue
  `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET` command after `DRIVER_OK`. Each
  receive queue is an `RxQueue` with its own buffer pool; `service`
  harvests every queue into its own shared receive ring. Transmit stays a
  single queue (the stack serialises egress). A single-queue device uses
  one `RxQueue` at index 0, unchanged.
- `no_std`, allocation-free steady state (staging carved once at `open`).
- Fail-closed: a runt/oversize/corrupt TX slot is dropped without wedging
  the queue; a device fault is a typed `DriverError`, never a panic
  (`AGENTS.md` §2.9).
- Zero-on-free: a `BufferClass::Sensitive` ring scrubs the persistent
  staging before reuse (`AGENTS.md` §4).

## Test surface

`cargo test -p tairix-virtio-net` drives the engine against the
`lib/virtio` `MockTransport`/`MockHost`: `open` reads the MAC, TX/RX
frame-ring round-trips, runt/oversize/corrupt-slot handling, the
non-blocking transmit doorbell (it never waits on the device) and its
single-staging back-pressure, `BufferClass::Sensitive` scrubbing, the
no-per-packet-DMA steady-state invariant, and mergeable receive buffers
(negotiation on/off, single-buffer over the 12-byte header, in-order
multi-buffer reassembly, the three fail-closed drops — zero /
out-of-range `num_buffers`, over-link-frame merge — and the pool
capturing a burst in one service), and multiqueue receive (a two-pair
device: `VIRTIO_NET_F_MQ` negotiation, the control-queue pair-count
handshake, and per-queue steering into each queue's own ring).
