# Network drivers

Link-layer drivers move raw Ethernet (or equivalent) frames and report
device facts. They implement
[`tairix_abi::driver::net::Net`](../abi/driver_traits.md). Higher
layers (ARP, IP, ICMP, …) live in the user-space `netstack` service
above this trait (`plans/NETWORK.md`) and are out of scope for the
driver class.

## Class trait

`Net` exposes three methods:

| Method          | Purpose                                              | Capability gate                |
|-----------------|------------------------------------------------------|--------------------------------|
| `device_facts`  | typed device report (MAC, MTU, link, offloads, queues) | `DriverHandle` ownership     |
| `service`       | pump the shared-memory frame rings once (non-blocking) | `CAP_NET_RAW` at dispatch site |
| `ack_interrupt` | deassert the device's interrupt line after an IRQ    | `DriverHandle` ownership       |

`device_facts` returns a `DeviceFacts` the consumer validates whole
(`validate()`, fail closed): the MTU must sit inside the
68..=65 535 bound and the receive-queue count must be at least 1, so a
corrupt or hostile report can never size an attacker-controlled
allocation.

## The frame-ring transport

Frame I/O is the shared-memory frame-ring transport
(`tairix_abi::driver::net_ring`): the stack owns a `FrameRings` pair —
queued transmits in `tx`, delivered frames in `rx` — and hands it to
`service`, the single doorbell that moves frames both ways. The rings
are mutated only *inside* the `service` call, so the call boundary is
the synchronisation and the whole transport is safe Rust; no frame
bytes cross the IPC when the region is shared between processes. Ring
state read back from the region is untrusted: corrupt counters or slot
lengths are refused (`BadMagic`), and a corrupt slot is consumed so it
cannot wedge the queue behind it.

Each slot also carries a small per-frame **offload descriptor**
(`FrameOffload`) — the transport-neutral analogue of a device's
per-descriptor header — read and written by `push_with`/`pop_with`
(`push`/`pop` are the no-offload path). A receive frame the device
checksum-validated is tagged `Validated`; one delivered with a partial
checksum is tagged `NeedsChecksum { csum_start, csum_offset }`. A transmit
frame the stack asked the device to checksum is tagged
`TxChecksum { csum_start, csum_offset }`. The tag decodes fail-closed (an
unknown byte is `None`), so a corrupt descriptor can only *lose* an
offload, never fabricate one, and the offload is never load-bearing for
security (`plans/NETWORK.md` N7a/N7b-1).

`service` semantics:

- Every frame queued in `tx` is moved into the device; a frame the
  device cannot move (runt, over-MTU, corrupt slot) is consumed and
  dropped so the queue keeps flowing.
- Delivered frames move into `rx` until the device is drained or the
  ring is full (`ServiceReport::rx_ring_full` — nothing is dropped;
  the stack drains and calls again).
- `service` is **non-blocking**: it drains whatever is ready and
  returns (an empty `ServiceReport` means "nothing yet", not "spin").
  Waiting for the *next* device event is the caller's job — the driver
  process parks on the device IRQ and `ack_interrupt`s it, then rings
  the stack's notify port. A blocking doorbell would wedge a
  cross-process `Service` reply, so the wait never lives here.

## Cross-process channel handoff (`net_channel`)

When the driver and the stack are separate processes — the true
microkernel shape — the `service` doorbell and the `FrameRings` region
are bridged by the versioned IPC control-plane contract
`tairix_abi::driver::net_channel` (`netchan-v1`). The driver owns the
device and serves a call endpoint; the stack is the client that owns the
region. This is the display service's `shm_grant` handoff with the roles
inverted and frames flowing both ways:

1. The stack asks `Facts` for the device's `DeviceFacts` and sizes a
   `RingGeometry` from the MTU.
2. It `shm_create`s the region, `shm_grant`s it to the driver's endpoint
   (the recipient is resolved kernel-side from the endpoint, never a
   recyclable PID), and sends `Attach { geometry, region_grant, class,
   notify_port }`; the driver `shm_map`s exactly that region
   (owner-checked — no ambient authority).
3. `Service` is the doorbell: the driver services the mapped rings once
   and replies a `ServiceReport`. Between doorbells the driver parks on
   its device IRQ and `ipc_send`s a `NetChannelNotify` to `notify_port`
   when receive frames arrive; the stack, parked on that port, issues the
   next `Service`. Neither side busy-polls.
4. `Detach` releases the channel.

Every frame in the contract decodes total and fail-closed (magic,
version, reserved-must-be-zero, geometry/class bounds,
`DeviceFacts::validate`) and is covered by the shared `lib/abi`
never-panic fuzz harness. The contract adds no capability and no syscall:
it is built on the existing `shm_create`/`shm_grant`/`shm_map`, endpoint,
and `irq_wait` primitives.

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
| [virtio-net](./virtio.md) | `tairix-drv-network-virtio-net` | virtio (PCI / MMIO) | ring transport + facts; receive-checksum offload (`VIRTIO_NET_F_GUEST_CSUM` → `RX_CSUM_VALIDATED`); TCP transmit-checksum offload (`VIRTIO_NET_F_CSUM` → `TX_CSUM_TCP`) |

The virtio-net device engine (`VirtioNet`) lives in `lib/virtio_net`
so a user-space **driver process** can link it without depending on a
`drivers/*` crate: `drivers/network/virtio_net_driver` is that process
— it brings the device up, claims a reserved device-channel endpoint,
emits the `netchan` hardware-tree node the device manager binds to the
stack, and serves the `netchan-v1` contract over a wait-set loop on
`{call endpoint, device IRQ}`. The device manager autobinds a
discovered `netchan` node to `netstack` (`plans/NETWORK.md` N4d).

### Receive-checksum offload

When the device offers `VIRTIO_NET_F_GUEST_CSUM` the driver negotiates it
and advertises `NetOffloads::RX_CSUM_VALIDATED`. It then reads each
receive frame's `virtio_net_hdr` flags and tags the ring slot: a
`VIRTIO_NET_HDR_F_DATA_VALID` frame is `Validated`, a
`VIRTIO_NET_HDR_F_NEEDS_CSUM` frame is `NeedsChecksum` carrying the
device's `csum_start`/`csum_offset`. The driver does **no** checksum
arithmetic — it never links `lib/net`, so the kernel that links the
driver crate (for the virtio-net device id) stays free of the stack. The
`netstack` service completes a `NeedsChecksum` frame through the one
`internet_checksum` and lets `lib/net` skip the redundant fold for a
`Validated` one, with every semantic check still running and the software
path as the byte-for-byte conformance oracle (`plans/NETWORK.md` N7a).

### Transmit-checksum offload (TCP)

When the device offers `VIRTIO_NET_F_CSUM` the driver negotiates it and
advertises `NetOffloads::TX_CSUM_TCP`. For a transmit frame the ring tags
`TxChecksum { csum_start, csum_offset }`, the driver builds the frame's
`virtio_net_hdr` with `VIRTIO_NET_HDR_F_NEEDS_CSUM` and those offsets, so
the device completes the fold over the transport bytes the stack left
partial; every other frame gets a zero header (its complete software
checksum is transmitted verbatim). As on receive, the driver does no
checksum arithmetic. UDP transmit offload is not advertised — the stack
keeps UDP's zero-checksum-as-`0xFFFF` rule on the software path. Because
the path is guest-driven, the existing TCP QEMU verticals exercise it once
`VIRTIO_NET_F_CSUM` is offered; QEMU recomputes the checksum on loopback
(`plans/NETWORK.md` N7b-1).

The netstack QEMU verticals
(`tests/integration/netstack_autoload_qemu_{aarch64,riscv64,x86_64}`)
drive a live emulated device end to end across the **two-process**
boundary: the production boot's bootstrap-floor discovery enumerates the
virtio-net node (over each arch's bus — virtio-MMIO on aarch64/riscv64
via `hwdiscovery::observe_virtio_mmio_network_devices`, virtio-PCI +
kernel-routed MSI-X on x86_64), `devmgr` autoloads the signed driver
bundle from `/System/Drivers/network/` into its own process, and it
serves `netstack`, which auto-configures the interface's EUI-64 IPv6
link-local and answers a host peer's link-local echo across the boundary
(`plans/NETWORK.md` N4e). All three arches are two-process; the earlier
single-process in-kernel netstack-engine verticals were removed once
that became true.
