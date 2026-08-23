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
- **Frame buffers**: 64 receive and 64 transmit buffers of 2 KiB — a single
  256 KiB carve taken once at `open` and reused for every frame. At line rate
  that is roughly 96 KB of 1500-byte frames in flight each way.
- **Receive**: descriptors are armed once with `DMA_OWN` and never rewritten;
  the consumer index alone hands a slot back, so the hot path writes one
  register per frame. `RBUF_ALIGN_2B` is enabled, so a frame starts two bytes
  into its buffer and the reported length includes the pad. `CMD_CRC_FWD` is
  left clear, so the MAC strips the frame check sequence.
- **Transmit**: each descriptor's buffer address is written once at bring-up;
  the per-frame path writes only `length_status` (length, queue tag,
  `APPEND_CRC`, `SOP`, `EOP`) and rings the producer index.
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

`NetOffloads::empty()`. The GENET has checksum and segmentation engines, but
a driver may advertise only what it has *verified* it can do and this device
is not emulable, so claiming them would be a claim about untested silicon.
The stack's software path is the canonical implementation, so the NIC is
complete without them.

## The device is not trusted

A buggy or wedged controller is inside the fault boundary the driver defends,
so its ring reports are validated rather than believed. A transmit consumer
index claiming more descriptors than were ever queued, or a receive producer
index claiming more completed descriptors than the ring holds, is refused with
`DeviceFault` — honouring either would free slots still in flight or deliver
whatever the descriptor RAM happened to hold. A frame the ring offers that is
wider than a device buffer is released and dropped rather than left to wedge
the queue behind it.

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
buffer is scrubbed when the device has consumed it, and a receive buffer
after its frame has been delivered and before its slot is handed back. The
caller-owned frame rings are not zeroed; that stays the caller's
responsibility.

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
