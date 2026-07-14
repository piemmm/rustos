# `rustos-virtio-net` — virtio-net device engine

Stability tier: **experimental**.

Arch-neutral, transport-agnostic virtio-net link-layer device logic: the
bring-up (virtio 1.1 §3.1 init sequence, `VIRTIO_NET_F_MAC`) and the
frame-ring `rustos_abi::driver::net::Net` service (drain the TX ring into
the device, harvest delivered frames into the RX ring), written once over
the bus-agnostic `lib/virtio` transport so the same source compiles
against the PCI (x86_64) and MMIO (aarch64, riscv64) backends.

This is a `lib/*` device-logic crate, the `lib/virtio_input` precedent.
Living in `lib/*` — rather than in the `drivers/network/virtio_net`
crate — is what lets a user-space driver *process* link the engine
directly: a process crate may depend on `lib/*` but never on another
`drivers/*` crate (`AGENTS.md` §17.4). The driver-host registration shell
(`register`) that wraps this engine is the `rustos-drv-network-virtio-net`
crate, which re-exports `VirtioNet` from here.

- `no_std`, allocation-free steady state (staging carved once at `open`).
- Fail-closed: a runt/oversize/corrupt TX slot is dropped without wedging
  the queue; a device fault is a typed `DriverError`, never a panic
  (`AGENTS.md` §2.9).
- Zero-on-free: a `BufferClass::Sensitive` ring scrubs the persistent
  staging before reuse (`AGENTS.md` §4).

## Test surface

`cargo test -p rustos-virtio-net` drives the engine against the
`lib/virtio` `MockTransport`/`MockHost`: `open` reads the MAC, TX/RX
frame-ring round-trips, runt/oversize/corrupt-slot handling, a
shared-interrupt spurious-wake mid-transmit, `BufferClass::Sensitive`
scrubbing, and the no-per-packet-DMA steady-state invariant.
