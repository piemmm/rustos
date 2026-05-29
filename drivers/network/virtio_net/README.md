# `rustos-drv-network-virtio-net` — virtio-net link-layer driver

Stage 4 deliverable. Implements `rustos_abi::driver::net::Net` on top
of the cross-arch virtio transport in `drivers/bus/virtio`. As with
`virtio_blk`, the driver is **bus-agnostic** — the same source
compiles against the PCI (x86_64) and MMIO (aarch64, riscv64) backends.

## Wire protocol

virtio 1.1 §5.1 — Stage 4 implements the legacy unextended subset:

- Receive queue at index 0, transmit queue at index 1.
- 10-byte `struct virtio_net_hdr` prefix on every chain, always zero
  (no offloads negotiated). `VIRTIO_NET_F_MRG_RXBUF`,
  `VIRTIO_NET_F_CSUM`, `VIRTIO_NET_F_GUEST_TSO*` etc. are Stage 5
  follow-ups.
- `VIRTIO_NET_F_MAC` is honoured: the link-layer address is read
  verbatim from the device-configuration window.
- Minimum frame size 14 bytes (Ethernet header); MTU = 1500 + 14.

Higher-layer protocols (ARP, IP, ICMP, …) live above the `Net` trait
in user space and are out of scope for `abi-v1`.

## Supported hardware

| Bus      | Architectures              | Stage 4 status                 |
|----------|----------------------------|---------------------------------|
| virtio   | x86_64 / aarch64 / riscv64 | mock-transport only (see notes) |

Same caveat as `virtio_blk`: real hardware drive requires the
kernel-side DMA mapping, IRQ routing, and bus-handle hand-off
described in `.junie/next-session-prompt.md` items 1–3.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `Net::transmit` / `Net::receive` (and their `_with_class` variants)
  additionally require the dispatcher to have verified `CAP_NET_RAW`,
  per `lib/abi/src/driver/net.rs`.

## Zero-on-free

`Net::transmit_with_class(_, BufferClass::Sensitive)` and
`Net::receive_with_class(_, BufferClass::Sensitive)` route every
internal staging copy through `BounceBuffer`, whose `Drop` impl
scrubs the DMA region before release (`AGENTS.md` §4). The
caller-owned `frame` / `buf` is **not** zeroed; that scrubbing
remains the caller's responsibility.

## Test surface

`cargo test -p rustos-drv-network-virtio-net` exercises:

- `open` reads MAC from the device-configuration window.
- `transmit` round-trips an Ethernet frame through the in-process
  peer.
- `receive` returns a queued frame; idle queue returns `Ok(0)`.
- Frame-size validation (undersize → `BufferTooSmall`, oversize →
  `LengthOutOfRange`).
- Empty receive buffer rejected.
- `BufferClass::Sensitive` transmit + receive round-trip.
- `register` capability gate.

9/9 host-side tests pass; ARP + ICMP round-trip against
`qemu user net` requires the userland networking stack documented as
item 5 of `.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`VirtioNet` type is re-exported so the driver host can construct an
instance; the host never reaches into the type beyond the `Net`
trait surface.
