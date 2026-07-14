# `rustos-drv-network-virtio-net` — virtio-net link-layer driver

Stage 4 deliverable. The bus-agnostic device engine (bring-up +
frame-ring `rustos_abi::driver::net::Net` service) lives in the
`lib/virtio_net` crate and is **re-exported** here as `VirtioNet`; this
crate is the driver-host registration shell (`register`) over it. The
engine is hoisted into `lib/*` (not kept in this `drivers/*` crate) so a
user-space driver *process* can link it directly — a process crate may
depend on `lib/*` but never on another `drivers/*` crate (`AGENTS.md`
§17.4). As with `virtio_blk`, the same engine source compiles against
the PCI (x86_64) and MMIO (aarch64, riscv64) backends.

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

`cargo test -p rustos-drv-network-virtio-net` exercises the driver-host
`register` capability gate. The device-engine host tests (`open` reads
the MAC, TX/RX frame-ring round-trips, runt/oversize/corrupt-slot
handling, `BufferClass::Sensitive` scrubbing, and the no-per-packet-DMA
steady-state invariant) live with the engine in `lib/virtio_net`
(`cargo test -p rustos-virtio-net`).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`VirtioNet` type is re-exported so the driver host can construct an
instance; the host never reaches into the type beyond the `Net`
trait surface.
