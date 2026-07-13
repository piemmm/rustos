# Network drivers

Link-layer drivers move raw Ethernet (or equivalent) frames and report
device facts. They implement
[`rustos_abi::driver::net::Net`](../abi/driver_traits.md). Higher
layers (ARP, IP, ICMP, …) live in the user-space `netstack` service
above this trait (`plans/NETWORK.md`) and are out of scope for the
driver class.

## Class trait

`Net` exposes two methods:

| Method         | Purpose                                              | Capability gate                |
|----------------|------------------------------------------------------|--------------------------------|
| `device_facts` | typed device report (MAC, MTU, link, offloads, queues) | `DriverHandle` ownership     |
| `service`      | pump the shared-memory frame rings once              | `CAP_NET_RAW` at dispatch site |

`device_facts` returns a `DeviceFacts` the consumer validates whole
(`validate()`, fail closed): the MTU must sit inside the
68..=65 535 bound and the receive-queue count must be at least 1, so a
corrupt or hostile report can never size an attacker-controlled
allocation.

## The frame-ring transport

Frame I/O is the shared-memory frame-ring transport
(`rustos_abi::driver::net_ring`): the stack owns a `FrameRings` pair —
queued transmits in `tx`, delivered frames in `rx` — and hands it to
`service`, the single doorbell that moves frames both ways. The rings
are mutated only *inside* the blocking `service` call, so the call
boundary is the synchronisation and the whole transport is safe Rust;
no frame bytes cross the IPC when the region is shared between
processes. Ring state read back from the region is untrusted: corrupt
counters or slot lengths are refused (`BadMagic`), and a corrupt slot
is consumed so it cannot wedge the queue behind it.

`service` semantics:

- Every frame queued in `tx` is moved into the device; a frame the
  device cannot move (runt, over-MTU, corrupt slot) is consumed and
  dropped so the queue keeps flowing.
- Delivered frames move into `rx` until the device is drained or the
  ring is full (`ServiceReport::rx_ring_full` — nothing is dropped;
  the stack drains and calls again).
- When nothing moved at all, the driver parks once on its host's
  device-event waiter and re-checks, so a caller looping on `service`
  is event-driven, never a spin.

## `BufferClass` and zero-on-free

`FrameRings::class` declares the traffic's sensitivity for the whole
ring set. When it is `Sensitive` the driver zeroes every internal
staging copy before `service` returns (`AGENTS.md` §4); a harvested
frame still awaiting a free RX slot is scrubbed after it is delivered,
never before. The trait makes no guarantee about scrubbing the ring
region itself; that remains the stack's responsibility.

## Shipped drivers

| Driver                    | Crate                           | Supported buses     | Status                                             |
|---------------------------|---------------------------------|---------------------|----------------------------------------------------|
| [virtio-net](./virtio.md) | `rustos-drv-network-virtio-net` | virtio (PCI / MMIO) | ring transport + facts; no offloads negotiated yet |

The virtio-net QEMU verticals (`tests/integration/virtio_net_*`) drive
a live emulated device end to end: ARP-resolve the SLIRP gateway and
exchange one ICMP echo through the ring transport.
