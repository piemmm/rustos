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
`TxChecksum { csum_start, csum_offset }`; one it asked the device to
segment (TSO) is tagged `TxSegment { csum_start, csum_offset, gso_size,
hdr_len, ipv6 }`. The tag decodes fail-closed (an unknown byte is `None`),
so a corrupt descriptor can only *lose* an offload, never fabricate one,
and the offload is never load-bearing for security (`plans/NETWORK.md`
N7a/N7b-1/N7b-2).

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
   `RingGeometry` from them with the shared `RingGeometry::for_device`:
   the receive ring holds one link frame (MTU + Ethernet header), and the
   transmit ring holds a segmentation-offload super-frame (up to
   `MAX_SLOT_CAPACITY`) when the device negotiated `TX_SEGMENT_TCP`, else
   the same as receive. `Attach` carries both capacities and the driver
   re-derives the minima to validate the offer.
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
| [virtio-net](./virtio.md) | `tairix-drv-network-virtio-net` | virtio (PCI / MMIO) | ring transport + facts; receive-checksum offload (`VIRTIO_NET_F_GUEST_CSUM` → `RX_CSUM_VALIDATED`); TCP transmit-checksum offload (`VIRTIO_NET_F_CSUM` → `TX_CSUM_TCP`); TCP segmentation offload (`VIRTIO_NET_F_HOST_TSO4`+`TSO6` → `TX_SEGMENT_TCP`); mergeable receive buffers (`VIRTIO_NET_F_MRG_RXBUF`); multiqueue receive (`VIRTIO_NET_F_MQ` + `VIRTIO_NET_F_CTRL_VQ`) |
| GENET v5                  | `tairix-drv-network-genet`       | platform MMIO (aarch64) | ring transport + facts; MDIO/PHY autonegotiation at 10/100/1000; no offloads advertised (see below) |

Both driver *processes* share one control plane: `lib/netchan` carries the
`netchan-v1` server (`NetChannelServer`) and the process loop that claims a
reserved device-channel endpoint, emits the `netchan` hardware-tree node the
device manager binds to the stack, and parks on `{call endpoint, device IRQ}`.
The device manager autobinds a discovered `netchan` node to `netstack`
(`plans/NETWORK.md` N4d). Each driver is therefore device bring-up plus one
`netchan::serve` call.

The virtio-net device engine (`VirtioNet`) lives in `lib/virtio_net` so the
kernel's bootstrap-floor discovery can share its device id;
`drivers/network/virtio_net_driver` is the process that links it.

### GENET v5 (Raspberry Pi 4B on-board gigabit Ethernet)

`drivers/network/genet` drives the BCM2711's `brcm,bcm2711-genet-v5` UniMAC
and the external BCM54213PE RGMII PHY behind its embedded MDIO master. Unlike
virtio-net it is one crate with both targets — a host-testable `lib` and the
`Run` binary — because a NIC sits above the bootstrap floor and so has no
charter-legal non-driver consumer for a `lib/*` device-support crate
(`AGENTS.md` §2.22).

Its descriptor rings live in the controller's **own on-chip RAM** inside the
register aperture, so the only DMA is frame buffers: one 256 KiB carve holding
64 receive and 64 transmit buffers of 2 KiB, taken once at bring-up and reused
for every frame. Receive descriptors are armed once and never rewritten — the
consumer index alone hands a slot back — so the hot path writes one register
per frame.

Bring-up refuses any core that does not report the GENET v5 revision, masks
the level-2 interrupt controller wholesale *before* programming anything, and
afterwards unmasks exactly `{RXDMA done, TXDMA done, link up, link down}`. A
link event re-resolves and re-programs the negotiated link on the next service
doorbell, so a cable change needs no driver restart.

It advertises **no offloads**. The GENET has checksum and segmentation
engines, but a driver may advertise only what it has *verified* it can do
(`plans/NETWORK.md` §0) and QEMU models no GENET, so claiming them would be a
claim about untested silicon. The stack's software path is the canonical
implementation, so the NIC is complete without them.

#### The link-layer address comes from discovery

The Pi's factory MAC is not readable from the GENET's registers: the platform
firmware publishes it through the device tree's
`mac-address` / `local-mac-address` ethernet-controller binding. The hardware
tree carries it to the driver as a `HwResourceKind::LinkAddress` resource on
the matched node — the one carrier that reaches a driver *process*, since
`resource_grants` delivers resources rather than node snapshots. It is a
discovered *fact*, not a handle: `HwResourceKind::required_capability` reports
`None` for it, so holding it authorises nothing. A node that publishes no
address fails the bring-up closed; a NIC on an invented address would answer
to the wrong DHCP reservation and form the wrong IPv6 link-local.

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

### Transmit segmentation offload (TSO)

When the device offers **both** `VIRTIO_NET_F_HOST_TSO4` and
`VIRTIO_NET_F_HOST_TSO6` (on top of `VIRTIO_NET_F_CSUM`, which segmentation
requires) the driver negotiates them and advertises
`NetOffloads::TX_SEGMENT_TCP`; both are required because the stack's
offload is IP-family-neutral. For a `TxSegment` frame the driver builds a
GSO `virtio_net_hdr` — `gso_type` `TCPV4`/`TCPV6`, `hdr_len`, `gso_size`,
and `VIRTIO_NET_HDR_F_NEEDS_CSUM` plus the checksum offsets — so the device
splits the one over-size segment into MTU-sized packets, advancing the
sequence number and completing each segment's checksum. The driver's
transmit staging is sized to the transmit-ring slot capacity (the GSO cap)
when TSO is negotiated. As elsewhere, the driver does no
checksum/segmentation arithmetic; `lib/net`'s software segmentation is the
byte-for-byte conformance oracle (`plans/NETWORK.md` N7b-2).

### Mergeable receive buffers (`MRG_RXBUF`)

When the device offers `VIRTIO_NET_F_MRG_RXBUF` the driver negotiates it.
Two things change. First, the driver posts a **pool** of receive buffers
rather than a single outstanding one, so a burst of frames the device
delivers back to back is captured before the stack next services the ring
— the single-buffer predecessor could hold only one, and the device
dropped the rest. Second, the `virtio_net_hdr` grows to the 12-byte
`virtio_net_hdr_mrg_rxbuf` on **both** rings (a transitional device sizes
the header uniformly once mergeable is on), and the device may deliver one
frame across several receive buffers, recording the count in the first
buffer's `num_buffers`. The driver reassembles those buffers in order into
one frame: a ≤MTU frame arrives in a single buffer (`num_buffers` == 1)
and is delivered straight from it, while a merged frame is assembled
through a reassembly buffer bounded to one link frame. Reassembly is
fail-closed — a zero or out-of-range `num_buffers`, a completion naming no
posted buffer, a runt shorter than the header, or a merge that would
exceed one link frame drops the frame (never a fabricated one, never an
out-of-bounds access) and the driver keeps flowing. Buffers are re-posted
to the device once their frame is delivered, and scrubbed first when the
ring class is sensitive (`plans/NETWORK.md` N7c).

### Multiqueue receive (`VIRTIO_NET_F_MQ`)

When the device offers both `VIRTIO_NET_F_MQ` and `VIRTIO_NET_F_CTRL_VQ`
and advertises more than one queue pair, the driver enables multiqueue
receive: it reads `max_virtqueue_pairs` from device config, brings up one
receive + one transmit virtqueue per enabled pair (bounded by the
transport's `MAX_RX_QUEUES` = 8), sets up the control virtqueue, and
issues `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET` after `DRIVER_OK` to select the
pair count. The shared frame region then carries one receive ring per
enabled queue (`RingGeometry::rx_queues`, `FrameRings::rx_ring(i)`)
followed by a single transmit ring — the stack serialises its own egress,
so transmit stays one queue. The device steers each received frame into
one of its receive queues; `service` harvests every queue into its own
receive ring, and `netstack` drains all of them into its single stack, so
a busy link's receive work is spread rather than serialised behind one
queue. Each queue owns an independent buffer pool + reassembly buffer, so
queues never share device-visible memory; the idle transmit queues of the
enabled pairs are configured (virtio requires every queue of an enabled
pair to be set up before the count is selected) and then held. A
single-queue device uses exactly one receive ring at index 0 and is
unchanged. `device_facts.rx_queues` reports the enabled count.

The path is guest-driven and proved by the `lib/virtio_net`
`multiqueue_enables_queues_and_steers_receive_per_queue` host test (a
two-pair device: control-queue handshake, per-queue steering into its own
ring). No live QEMU vertical presents multiqueue: the net verticals use
the `-netdev dgram` socket backend, which QEMU restricts to a single
queue (multiqueue needs a `tap` netdev with `queues=N`), so a dgram-backed
guest sees `max_virtqueue_pairs = 1` and correctly stays single-queue —
the host test is the authoritative proof, exactly as for the other
offloads (`plans/NETWORK.md` N7c-2).

### Per-architecture offload state

The offload set above is a property of the arch-neutral `virtio_net`
driver and the `lib/net` engine, not of any one target: the same four
negotiated offloads (receive checksum, TCP transmit checksum, TCP
segmentation, mergeable receive buffers) are available identically on
`x86_64`, `aarch64`, and `riscv64` — the only per-arch difference is the
bus the device is discovered over (virtio-PCI vs. virtio-MMIO), which does
not change the offload contract. `wasm32` has no network device. The
README support matrix records this as one "Network offloads" row
(`✓ virtio` on the three device-bearing targets, `—` on `wasm32`). Each
offload is negotiated only when the device offers it and is never
load-bearing for correctness — the `lib/net` software path is the
byte-for-byte oracle for every one. The engine's data-plane receive and
transmit paths additionally allocate nothing on the heap in steady state
(a reused output recycles buffers through a bounded pool), a §2.16
performance budget enforced by the `lib/net` `hotpath_allocations`
regression test (`plans/NETWORK.md` N7c-3).

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
