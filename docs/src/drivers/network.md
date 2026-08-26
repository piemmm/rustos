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
| `device_facts`  | typed device report (MAC, MTU, link, offloads, queues, group filter) | `DriverHandle` ownership |
| `service`       | pump the shared-memory frame rings once (non-blocking) | `CAP_NET_RAW` at dispatch site |
| `set_multicast_groups` | replace the group destinations the device admits | `CAP_NET_RAW` at dispatch site |
| `ack_interrupt` | deassert the device's interrupt line after an IRQ    | `DriverHandle` ownership       |

`device_facts` returns a `DeviceFacts` the consumer validates whole
(`validate()`, fail closed): the MTU must sit inside the
68..=65 535 bound, the receive-queue count must be at least 1, and
`max_tx_frame` — the largest single transmit frame the driver will stage —
must hold at least one link frame and no more than the transport's slot
ceiling, so a corrupt or hostile report can never size an
attacker-controlled allocation. It also declares how the device filters
group (multicast) destinations, which is what tells the stack whether it
must program the device's filter at all.

## The frame-ring transport

Frame I/O is the shared-memory frame-ring transport
(`tairix_abi::driver::net_ring`): the stack owns a `FrameRings` pair —
queued transmits in `tx`, delivered frames in `rx` — and hands it to
`service`, the single doorbell that moves frames both ways. The rings
are a genuine **single-producer, single-consumer** pair: a driver process
harvests into the receive ring from its own device interrupt while the
stack drains it, so the doorbell is *not* the synchronisation. The
producer/consumer counters are `AtomicU32`s with a release-publish /
acquire-observe discipline and sit in separate cache lines — in one line
every publish would invalidate the peer's read of the other and the two
CPUs would ping-pong it per frame. No frame bytes cross the IPC when the
region is shared between processes.

The region must be aligned for those counters. A cross-process region is
`shm`-mapped and page-aligned already; `aligned_region` and
`REGION_ALIGN_PADDING` cut an aligned view from a plain in-process buffer,
and `bind` refuses a misaligned one (`BadAlignment`).

Ring state read back from the region is untrusted, and *both* counters live
in memory the peer can write: every operation snapshots them once and
validates the occupancy before it addresses a slot, so a corrupt counter or
slot length is a typed error rather than an out-of-bounds read, and a
corrupt slot is consumed so it cannot wedge the queue behind it.

### The receive pre-filter

A frame enters a receive ring through one call, `FrameRings::deliver`,
which consults the installed `RxAdmit` pre-filter *before* copying it. That
is where a frame with no possible local consumer is shed. One implementation
serves every driver, so none repeats it (`plans/NETWORK.md` N17d).

The classifier **mirrors the stack's own destination-acceptance rule**: an
address of the interface, a group it has joined (plus all-nodes and the
solicited-node groups, derived from those addresses rather than carried
separately), or an IPv4 broadcast address whose UDP destination port a local
datagram consumer holds. A group destination is gated on membership alone,
so nothing about it can fall behind a socket opening.

Broadcast is the one destination whose acceptance depends on a *transport*
consumer rather than on an address or a membership: it reaches every host on
the segment, and a host with nothing bound to the port drops the datagram
anyway. The policy therefore carries a 512-bit summary of the local ports a
broadcast datagram could reach, republished as the service's datagram
sockets come and go, and the stack gates its own acceptance on the same
summary — so the two rules stay one rule. The summary's error is one-sided:
two ports can fold to one slot, so the filter may admit a frame the stack
then finds no consumer for, but it can never shed one a consumer wanted. A
running DHCPv4 client's port is in the summary, which is what lets its
broadcast reply through before the interface has any address at all. A
broadcast *fragment* is admitted without inspection (the port lives in the
first fragment, and reassembly is the stack's job); anything but UDP to a
broadcast destination is refused outright, because the stack consumes a
broadcast destination as UDP or not at all — a broadcast TCP segment is one
RFC 1122 requires a host to discard, and an echo request to a broadcast
address is the smurf amplifier.

It pays twice over. The shed frame is never copied, and a harvest that
admits nothing leaves `moved` false, so the driver sends no notify and the
stack is not woken at all. A shed frame does still count as *progress* for
the drain — `ServiceReport::harvested` counts what the device handed over
whether or not it reached the ring, and `DrainStep` keys on that rather than
on `received`. Otherwise a filter doing its job would make every pass look
quiet, re-arm the completion sources, and give back the burst coalescing
above one interrupt at a time. On an idle Raspberry Pi 4 the stack was being
woken for 7994 frames a minute and discarding 7917 of them.

Its bias is to **admit**: the stack still validates every frame it does
receive, and the driver already owns the device and could drop whatever it
liked, so refusing here grants nothing. Anything the classifier cannot
parse confidently is admitted, and a policy that could not name every
address or group widens rather than shedding something it was not told
about. `ServiceReport::filtered` counts what was shed, cumulatively, and
surfaces as `stats:net/<iface>/rx.filtered`.

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
   `RingGeometry` from them and the discovered machine with the shared
   `RingGeometry::for_device`. Every term is derived, never hand-picked
   (see [Ring sizing](#ring-sizing) below): the receive ring holds one link
   frame (MTU + Ethernet header); the transmit ring holds a
   segmentation-offload super-frame when the device negotiated
   `TX_SEGMENT_TCP`, capped by the driver's own `max_tx_frame` staging
   bound; and the two directions carry independent, power-of-two slot
   counts. `Attach` carries both counts and both capacities, and the driver
   validates the offer against its *own* facts — one link frame each way as
   the floor, its staging bound as the transmit ceiling — so a geometry it
   could not serve is refused at attach rather than dropping frames later.
2. It `shm_create`s the region, `shm_grant`s it to the driver's endpoint
   (the recipient is resolved kernel-side from the endpoint, never a
   recyclable PID), and sends `Attach { geometry, region_grant, class,
   notify_port }`; the driver `shm_map`s exactly that region
   (owner-checked — no ambient authority).
3. `SetRxFilter` publishes the local addresses, joined groups, and
   broadcast-consumer ports the driver's receive pre-filter matches
   against; the stack re-sends it whenever an interface's address set, its
   memberships, or the service's datagram sockets change, and never on the
   frame path.
4. `Service` is the doorbell: the driver services the mapped rings once
   and replies a `ServiceReport`. It is **not** on the receive path — a
   driver woken by its device interrupt masks its completion sources,
   harvests into the shared ring itself, and `ipc_send`s one
   `NetChannelNotify` carrying the live link, a back-pressure flag, and the
   device's cumulative receive-pre-filter count. The stack reads the ring
   locally and rings the doorbell only when the device has work this side
   created (something in the transmit ring) or the notify said a source is
   masked awaiting release. A pure receive therefore costs no call at all
   (`plans/NETWORK.md` N17a–b). The count rides the notify for exactly that
   reason: a receive-only interface rings no doorbell, so a figure carried
   only on the `Service` reply would leave `stats:net/<iface>/rx.filtered`
   frozen at whatever the last transmit observed. Neither side busy-polls.
5. `SetMulticast` replaces the group addresses the device admits. Sent
   only to a device whose `DeviceFacts::multicast_filter` is
   `McastFilter::Slots(n)` — an `Unfiltered` device already delivers every
   group frame, so it costs no IPC — and only when the stack's membership
   has changed, which it tracks with a revision rather than by rebuilding
   and diffing the set each doorbell. The set is replaced whole, so the
   stack's membership stays the single authoritative copy. A set larger
   than the device's slots is refused whole, the previously admitted set
   stays in force, and the stack audits the refusal: the filter is never
   widened (least of all to promiscuous) to make an over-large set fit.
6. `Detach` releases the channel.

### Ring sizing

No ring depth on the system is a hand-picked constant. A frame ring is
pinned memory that no reclaim can take back, and a depth that suits a
128 MiB board starves a server while one that suits a server exhausts the
board — so every depth is a function of what the boot path discovered.

`RingBudget::for_machine` states the budget once: one 4096th of the
installed RAM per ring, from the kernel-attested `BootFacts`. `slots` then
returns the deepest power-of-two ring that fits it (a power of two so slot
addressing masks the free-running counter instead of dividing by it, and
because every device ring the transport mirrors requires one anyway),
floored at `MIN_SLOTS` so the smallest machine still gets a working ring
and clamped at `MAX_SLOTS` so the largest gets a bounded one. A machine the
caller could not attest yields a zero budget and hence the floor —
deliberately, so an unattested host gets the minimum rather than an
invented figure.

`RingGeometry::for_device` composes that with the device's own report:

- **Receive queues** — the device's `rx_queues`, capped by the machine's
  core count and by `MAX_RX_QUEUES`. Receive steering exists to spread work
  across cores, so queues beyond the cores that could drain them only pin
  memory and lengthen every drain pass.
- **Slot capacities** — a receive slot holds one link frame. A transmit
  slot is widened to the largest super-frame the budget affords, capped by
  `max_tx_frame`, only when the device negotiated `TX_SEGMENT_TCP`. A board
  that cannot afford two 64 KiB slots gets a smaller super-frame — still a
  segmentation win — rather than a ring it cannot pay for.
- **Slot counts** — each direction takes the deepest ring its budget
  affords *at its own slot cost*. This is why the two counts are
  independent: one shared count would either waste a 64 KiB super-frame's
  space on every receive slot or shrink receive depth to what the
  super-frames cost, and a shallow receive ring back-pressures the driver
  at any real frame rate.

The drivers size their own device rings through the same policy, from the
machine the driver host attests (`DriverHost::machine`). GENET derives its
programmed descriptor count and segmentation staging area (`DmaLayout`,
bounded above by the controller's own descriptor RAM); `virtio_net` derives
its receive-buffer pool depth and transmit staging depth (`QueueDepths`,
bounded above by whatever the device advertises as its queue maximum and by
the crate's pinned-DMA bounds). Both sides therefore land at coherent
depths without either having to guess what the other could afford.

### Interrupt masking, and why it exists

A DMA engine's completion status latches a **level** condition ("completed
descriptors are waiting"). Acknowledging clears the latch, but with frames
still undrained the condition re-latches at once, and the kernel re-arms
the interrupt line every time the driver process parks. A driver that only
acknowledged and notified therefore spun interrupt → acknowledge → notify →
park at the speed of a context switch until the stack caught up —
measurable as a permanently busy core on an otherwise idle machine.

`Net::set_completion_interrupts` is the fix: the serve loop masks the
device's data-path completion sources on entry and unmasks them only once
the device is empty *and* the shared receive ring has room. A burst then
costs one interrupt instead of one per frame, which is the coalescing a
fixed frame threshold cannot give — so a device's own completion threshold
is programmed at its most responsive (GENET writes `MBUF_DONE_THRESH = 1` on
both rings with the ring timer disarmed, as Linux's `bcmgenet` does), and the
coalescing comes from the masking, which adapts to the actual burst where a
timer would only add latency to a lone frame. **Link and configuration-change
sources are never masked**, or a cable pulled mid-flood would go unnoticed.

That threshold is programmed, never inherited. It is the comparison that
raises the completion source, so a ring left holding whatever value reset or
the firmware put there has unknown interrupt behaviour — and a threshold of
zero is satisfied permanently, which is a level condition draining cannot
clear and therefore a storm no masking can end.

The policy itself is pure and host-tested: `DrainStep` classifies one report,
and `Drain` is the whole interrupt-path state machine over a sequence of
them. `Masked` names the state it can stop in — `BackPressure` (the shared
ring filled), `BudgetSpent` (the round bound ran out while the device still
had work), or `Fault` — and all three set the notify's back-pressure flag,
because in all three only the stack's next `Service` can release or diagnose
the sources. A drain that stopped masked without saying so would leave the
interface receiving nothing until some unrelated transmit happened to
release it.

The invariant that makes this safe is that the sources are never left masked
with nothing scheduled to lift them. On the interrupt path the notify's
back-pressure flag is that schedule. A `Service` doorbell has no such
channel — its reply's `rx_ring_full` asks the stack for nothing — so that
path re-arms unless the device faulted, and a source still asserted simply
re-interrupts into the path that does carry a release.

The sources are also masked whenever the channel is detached, so a device
left running cannot storm a driver with nowhere to put frames.

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
| GENET v5                  | `tairix-drv-network-genet`       | platform MMIO (aarch64) | ring transport + facts; MDIO/PHY autonegotiation at 10/100/1000; unicast + broadcast + 15-slot group receive filter; completion-interrupt masking; receive-checksum offload (`RBUF_RXCHK_EN` → `RX_CSUM_VALIDATED`); transmit-checksum offload (`TBUF_64B_EN` status block → `TX_CSUM_TCP`+`TX_CSUM_UDP`); driver-side TCP segmentation (`TX_SEGMENT_TCP`, no hardware engine — see below) |

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
**both** level-2 interrupt instances wholesale *before* programming anything
— the driver drives only the default ring, so it never binds `INTRL2_1`'s
line and never services it, and leaving those sources live would leave part
of the device's interrupt state to whatever the firmware left behind — and
afterwards unmasks exactly `{RXDMA done, TXDMA done, link up, link down}` on
`INTRL2_0`. A link event re-resolves and re-programs the negotiated link on
the next service doorbell, and that doorbell's report states the resolved
link, so a cable change reaches the stack (and a bond's failover) without a
driver restart.

#### The receive destination-address filter

The UniMAC's address registers identify the station for MAC control frames;
the receiver's own destination-address filter (`UMAC_MDF_CTRL` plus its
address slots) is what admits an arriving frame, and a controller left with
every slot disabled delivers **nothing**. Bring-up therefore enables exactly
two slots before it enables the receiver — the broadcast address and this
station's unicast address — the minimum a host needs to be addressable: ARP,
a DHCP offer, and a unicast reply. Programming it after `CMD_RX_EN` would
leave a window in which every arriving frame is dropped, so the ordering is
asserted by a test.

Promiscuous reception is deliberately never enabled. It would admit every
frame on the segment, including those addressed to other hosts — authority
the network stack has no reason to hold.

Group addresses take the slots after those two. The driver reports the 15 it
has left as `McastFilter::Slots(15)` in its `DeviceFacts`, and the stack
programs them through `Net::set_multicast_groups` whenever its membership
changes — all-nodes, the solicited-node group of each IPv6 address, and every
joined group (`plans/NETWORK.md` N14d). A set larger than the table is refused
whole, leaving the previously admitted set in force, and `netstack` audits the
refusal: the groups that did not fit are genuinely not delivered, so it is
never silent.

It advertises `RX_CSUM_VALIDATED | TX_CSUM_TCP | TX_CSUM_UDP |
TX_SEGMENT_TCP`; the IPv4 header checksum has no engine on this MAC and is
never claimed. The "GENET offloads" section below is the detail.

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

### GENET offloads

The question this section used to record — whether `RBUF_64B_EN`'s 64-byte
receive status block can coexist with the `RBUF_ALIGN_2B` two-byte pad —
turned out not to need answering: **the receive checksum offload does not
need the status block.** `RBUF_CHK_CTRL = RBUF_RXCHK_EN` alone puts the
engine in front of the receiver, and it reports its verdict in bit 15 of the
*completed* descriptor (the bit that means "device owns this" before
completion), leaving the frame exactly where it was. The receive-buffer
layout is unchanged, so the risk the question named does not arise.

- **Receive.** A frame the engine parsed and verified is delivered
  `FrameOffload::Validated` and the stack skips the fold; a frame it did not
  parse arrives `FrameOffload::None` and keeps the software fold. The offload
  can therefore only ever *save* work — it can never admit an unchecked
  frame — which is why the narrower parsed verdict is preferred here to the
  status block's whole-frame sum.
- **Transmit checksum.** `TBUF_64B_EN` — the *transmit* buffer's own
  register, so it touches no receive path — prefixes each transmitted buffer
  with a 64-byte transmit status block. Its one live word carries a validity
  bit, the offset where the fold starts, the offset of the checksum field
  (both relative to the frame, the block excluded), and a UDP flag, without
  which RFC 768's "a computed zero is sent as `0xFFFF`" rule would not be
  applied and a 1-in-65 536 UDP datagram would go out with its checksum
  silently disabled. The controller consumes the block; it never reaches the
  wire. Offsets the frame does not bear out are refused and the frame is
  dropped — a partial checksum must never be transmitted.
- **Segmentation without a segmentation engine.** The GENET has none (the
  reference driver advertises scatter-gather, `HW_CSUM`, and `RXCSUM`, and
  no `TSO`), so the driver splits the super-frame itself through the shared
  `tairix_net::txoffload::TcpSegmenter` and hands each wire segment to the
  transmit checksum engine — the shape Linux's `net/core/tso.c` gives
  `mvneta`, `mvpp2`, and `fec`. That still buys the offload's real win: one
  ring slot and one stack transmit pass for tens of wire packets, with each
  segment's TCP checksum still done in hardware. The split itself is
  device-independent arithmetic (per-segment IP length and identification,
  IPv4 header checksum, TCP sequence, `FIN`/`PSH` on the last segment and
  `CWR` on the first, and the pseudo-header partial advanced to each
  segment's own length), so it lives in `lib/net` once and is proven there
  by folding each emitted segment against an independent oracle.
- **A full ring defers, never drops.** A super-frame needs one descriptor
  per segment. When the ring fills mid-split the remainder stays staged in
  the driver and resumes at the next doorbell — which the transmit-completion
  interrupt provokes — so nothing is lost and nothing spins, and no later
  frame overtakes the stream being split.

QEMU models no GENET, so the coverage is the register-level model suite in
the driver plus the segmenter's own tests in `lib/net`; what metal still owes
is the *measurement*, recorded as an acceptance item in `plans/PI.md`.

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
