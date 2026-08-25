# `tairix-drv-network-genet` — Broadcom GENET v5 link-layer driver

The Raspberry Pi 4B's on-board gigabit Ethernet. Binds the discovered
`brcm,bcm2711-genet-v5` node, brings the UniMAC and both DMA rings up over
its granted register window and frame-buffer carve, negotiates the external
RGMII PHY over the MAC's embedded MDIO master, and serves the `netchan-v1`
device-channel contract to `userland/net/netstack`
(`plans/NETWORK.md` N14).

Two targets in one crate: the host-testable device logic (`src/lib.rs`,
`src/regs.rs`, `src/mdio.rs`, `src/wiring.rs`) and the `Run` entry-point
binary (`src/main.rs`) installed as a signed `/System/Drivers/` bundle and
autoloaded into user space by `devmgr`. The device logic stays in the driver
because a NIC sits above the bootstrap floor and so has no charter-legal
non-driver consumer for a `lib/*` device-support crate (`AGENTS.md` §2.22).

## Supported hardware

| Device                       | Bus         | Architecture | Status                |
|------------------------------|-------------|--------------|-----------------------|
| BCM2711 GENET v5 + BCM54213PE | platform MMIO | aarch64    | mock-tested (see below) |

The core revision is checked at bring-up: a controller that does not report
the GENET v5 encoding in `SYS_REV_CTRL` is refused (`Unsupported`) rather
than driven against a register layout that may not describe it.

## Wire protocol

- **Descriptors are on-chip.** Both descriptor rings live in the controller's
  own RAM inside the register aperture (receive at `0x2000`, transmit at
  `0x4000`), so the only DMA is the frame buffers.
- **One ring per direction**, the descriptor-based default queue (ring 16);
  the 16 priority queues are unused.
- **Frame buffers**: 64 receive and 64 transmit buffers of 2 KiB, plus one
  64 KiB transmit staging area for the segmentation path — a single carve
  taken once at `open` and reused for every frame. At line rate the ring
  holds roughly 96 KB of 1500-byte frames in flight each way.
- **Receive filter**: the UniMAC's address registers identify the station for
  MAC control frames; the receiver's own destination-address filter (`MDF`) is
  what admits a frame, and a controller whose filter slots are all disabled
  delivers nothing. Bring-up enables exactly two slots — the broadcast address
  and this station's unicast address — which is the minimum a host needs to be
  addressable (ARP, a DHCP offer, a unicast reply). Promiscuous reception is
  never enabled: it would hand the network stack frames addressed to other
  hosts on the segment.
- **Group addresses** take the remaining 15 of the MDF's 17 slots. The driver
  reports them as `McastFilter::Slots(15)` and the stack programs the set it
  needs through `Net::set_multicast_groups`, replacing it whole each time its
  membership changes (`plans/NETWORK.md` N14d). A set larger than the table is
  refused whole — the working set stays, and the filter is never widened to
  make it fit.
- **Receive**: descriptors are armed once with `DMA_OWN` and never rewritten;
  the consumer index alone hands a slot back, so the hot path writes one
  register per frame. `RBUF_ALIGN_2B` is enabled, so a frame starts two bytes
  into its buffer and the reported length includes the pad. `CMD_CRC_FWD` is
  left clear, so the MAC strips the frame check sequence.
- **Transmit**: each descriptor's buffer address is written once at bring-up.
  A buffer holds the 64-byte transmit status block the checksum engine reads
  and then the frame, so the per-frame path writes the status block, the
  `length_status` word (both lengths, queue tag, `APPEND_CRC`, `DO_CSUM` when
  the frame carries a partial checksum, `SOP`, `EOP`), and rings the producer
  index.
- **Link**: standard IEEE 802.3 clause-22 autonegotiation over the UniMAC's
  MDIO master — reset, advertise 10/100/1000 half and full, resolve the
  negotiated mode from the partner's ability registers, then program
  `UMAC_CMD`'s speed selector and `EXT_RGMII_OOB_CTRL`. The board wires
  `rgmii-rxid`, so `ID_MODE_DIS` is set: the receive delay is the PHY's and
  the MAC adds none of its own. A link event on `INTRL2_0` re-resolves the
  link on the next service doorbell, so a cable change is followed without a
  driver restart.
- **The link-layer address is not in the controller.** The Pi's factory MAC is
  published by the platform firmware through the device tree's
  `mac-address` / `local-mac-address` ethernet-controller binding, carried to
  the driver as the matched node's `HwResourceKind::LinkAddress` grant. A node
  that publishes none fails bring-up closed: a NIC on an invented address
  would answer to the wrong DHCP reservation and form the wrong IPv6
  link-local.

## Offloads

`RX_CSUM_VALIDATED | TX_CSUM_TCP | TX_CSUM_UDP | TX_SEGMENT_TCP`. The IPv4
header checksum has no engine on this MAC and is never claimed.

- **Receive checksum.** `RBUF_RXCHK_EN` puts the checksum engine in front of
  the receiver. It parses the frame's L3/L4 headers and reports its verdict
  in bit 15 of the *completed* descriptor — the bit that means "device owns
  this" before completion. The frame itself is untouched, so the 64-byte
  receive status block (`RBUF_64B_EN`) is deliberately **not** enabled and the
  receive-buffer layout is exactly what it was without the offload: still the
  two-byte `RBUF_ALIGN_2B` pad and nothing else. A verified frame is delivered
  as `FrameOffload::Validated`; anything the engine did not parse simply keeps
  the stack's software fold, so the offload can only ever save work, never
  admit an unchecked frame.
- **Transmit checksum.** `TBUF_64B_EN` prefixes every transmitted buffer with
  a 64-byte transmit status block. Its one live word directs the engine at the
  transport checksum the stack left partial — a start offset and a field
  offset, both relative to the frame with the status block excluded — and
  carries the UDP flag, without which RFC 768's "a computed zero is sent as
  `0xFFFF`" rule would not be applied and a 1-in-65 536 UDP datagram would go
  out with its checksum silently disabled. The controller consumes the block;
  it never reaches the wire. Offsets the frame does not bear out are refused
  and the frame is dropped — a partial checksum must never be transmitted.
- **Segmentation.** The GENET has **no** segmentation engine (the reference
  driver advertises `NETIF_F_SG | NETIF_F_HW_CSUM | NETIF_F_RXCSUM` and no
  `NETIF_F_TSO`), so the driver splits a `FrameOffload::TxSegment` super-frame
  itself through the shared `tairix_net::txoffload::TcpSegmenter` and hands
  each wire segment to the transmit checksum engine — the same shape Linux's
  `net/core/tso.c` gives `mvneta`, `mvpp2`, and `fec`. The win is the
  offload's real one: one ring slot and one stack transmit pass for tens of
  wire packets, with each segment's TCP checksum still done in hardware. A
  super-frame the ring cannot absorb in one doorbell stays staged and resumes
  at the next — which a transmit-completion interrupt provokes — so a full
  ring defers segments rather than dropping them, and no later frame overtakes
  the stream being split.

The register programming is the published Broadcom map as carried by the
Linux `bcmgenet` driver; the split itself is device-independent arithmetic
proven host-side in `lib/net`. What still needs metal is the measurement, not
the correctness: `plans/PI.md` carries the on-metal acceptance item.

## The device is not trusted

A buggy or wedged controller is inside the fault boundary the driver defends,
so its ring reports are validated rather than believed. A transmit consumer
index claiming more descriptors than were ever queued, or a receive producer
index claiming more completed descriptors than the ring holds, is refused with
`DeviceFault` — honouring either would free slots still in flight or deliver
whatever the descriptor RAM happened to hold. A frame the ring offers that is
wider than a device buffer is released and dropped rather than left to wedge
the queue behind it, as is a segmentation descriptor the frame does not bear
out or whose segments would not fit a transmit buffer.

## Interrupts

The serve loop parks on the device interrupt; it never polls. `INTRL2_0` is
masked wholesale before bring-up touches the device, and only
`{RXDMA done, TXDMA done, link up, link down}` are unmasked afterwards — an
unhandled condition must not be able to re-assert the line faster than it can
be serviced. `ack_interrupt` clears exactly the sources that are asserted, so
the line is deasserted before it is re-enabled and can never storm.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered register window.
- `CAP_MEM_DMA` to carve the frame buffers.
- `CAP_IRQ_BIND` for the interrupt the serve loop parks on.
- `CAP_SHM`, `CAP_IPC_ENDPOINT`, `CAP_IPC_BIND_PRIVILEGED`, `CAP_HW_EMIT`,
  `CAP_LOG_EMIT` for the device-channel service (`lib/netchan`).

Runs in user space; it does not request `CAP_DRV_KERNEL`. Loadable and
unloadable at runtime.

## Zero-on-free

A `BufferClass::Sensitive` ring is honoured in both directions: a transmit
buffer is scrubbed when the device has consumed it, a receive buffer after its
frame has been delivered and before its slot is handed back, and the transmit
staging area as soon as a super-frame's split completes. The caller-owned
frame rings are not zeroed; that stays the caller's responsibility.

## Test surface

QEMU models no GENET (`-device help` has no such device, and its `raspi*`
machines hand the kernel no device tree), so there is no emulated vertical.
`cargo test -p tairix-drv-network-genet` drives the engine against a
register-level model of the controller: the revision gate, the reset/ring/
enable sequence and its ordering, MDIO framing and its fail-closed timeout,
PHY link resolution at every rate plus link-down and re-plug, transmit
descriptor encoding with producer/consumer accounting and ring wrap, receive
delivery past the alignment pad, every error/fragment/malformed drop, receive
back-pressure without loss, the fail-closed refusal of an impossible ring
report, and the sensitive-class scrubbing. The live path
on real silicon is an on-metal acceptance item (`plans/PI.md`).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. `Genet` is a
public type the `Run` binary instantiates through `wiring::open_discovered`;
it never reaches into it beyond the `Net` trait.
