# Network drivers

Link-layer drivers send and receive raw Ethernet (or equivalent)
frames. They implement
[`rustos_abi::driver::net::Net`](../abi/driver_traits.md). Higher
layers (ARP, IP, ICMP, …) live above this trait in user space and
are out of scope for `abi-v1`.

## Class trait

`Net` exposes three method families:

| Method                                | Purpose                                        | Capability gate                |
|---------------------------------------|------------------------------------------------|--------------------------------|
| `mac_address`                         | report link-layer address                      | `DriverHandle` ownership       |
| `transmit` / `receive`                | one-shot frame transfer                        | `CAP_NET_RAW` at dispatch site |
| `transmit_with_class` / `receive_with_class` | classed transfer (see below)            | `CAP_NET_RAW` at dispatch site |

Per `AGENTS.md` §2.9 every error path returns `Result<_, DriverError>`
— under-MTU frames map to `BufferTooSmall`, over-MTU frames map to
`LengthOutOfRange`, full transmit queues map to `Busy`, and missing
`CAP_NET_RAW` maps to `PermissionDenied`.

## `BufferClass` and zero-on-free

`*_with_class` accept a `BufferClass` (`NonSensitive` /
`Sensitive`). When `class == Sensitive` the driver is required to
zero every internal staging copy of the frame before the method
returns (`AGENTS.md` §4). The default implementations delegate to
the plain methods and are safe only for drivers that DMA directly
between the caller-owned slice and the wire; drivers that
bounce-buffer (e.g. `virtio_net` over the Stage 4 host-side
allocator) override them.

The trait makes no guarantee about scrubbing the caller-owned
`frame` / `buf`; that remains the caller's responsibility.

## Shipped drivers

| Driver                                   | Crate                                | Supported buses     | Stage 4 status                          |
|------------------------------------------|--------------------------------------|---------------------|------------------------------------------|
| [virtio-net](./virtio.md)                | `rustos-drv-network-virtio-net`      | virtio (PCI / MMIO) | host-side tests + mock transport only    |

A full ARP + ICMP round-trip against `qemu user net` requires:

1. Kernel DMA + IRQ routing (`.junie/next-session-prompt.md` items 1–2),
2. PCI / MMIO bus-handle hand-off (item 3),
3. The userland ARP / IP / ICMP responder (item 5).
