# Bus drivers

RustOS bus drivers enumerate the devices attached to a transport
(PCI, MMIO, virtio) and surface them to the userland driver host as
`BusDevice` records. They implement the single class trait
[`rustos_abi::driver::bus::Bus`] and nothing else — every other type
is `pub(crate)` per `AGENTS.md` §8.

## Stage-4 drivers in this class

| Crate                    | Platform              | Status   |
| ------------------------ | --------------------- | -------- |
| `lib/pci`                | x86_64 (PIO) / PCIe ECAM / BCM2711 windowed | Shipped (library) |
| `drivers/bus/pcie_brcm`  | Pi 4 (BCM2711 RC)     | User-space bus-driver crate (link bring-up engine + `Run` bin; host-proven); metal pending |
| `drivers/bus/mmio`       | aarch64 / riscv64     | Shipped  |
| `drivers/bus/virtio`     | cross-arch            | Stage 4.D |
| `drivers/bus/usb/xhci`   | generic xHCI host (Pi 4 VL805) | P10 protocol layers + HID enumeration (host-proven) |
| `drivers/bus/usb/vl805`  | Pi 4 (VL805 device)   | User-space bus-driver crate: firmware-reload policy + `Run` bin (reload firmware → emit `usb,xhci` node B; host-proven); metal pending |

## Capability model

All bus drivers ship as user-space `.rxe` modules
(`DriverKind::UserSpace`). The driver host requires the universal
`CAP_DRV_LOAD` grant at `register` time; enumeration through `Bus`
inherits that gate through the issued `DriverHandle`
(`AGENTS.md` §5.4 / §8).

Bus drivers **never** read or write hardware until the host first
calls into the `Bus` trait — `register` itself is a pure capability
check that issues the per-driver marker handle.

## BAR / MMIO mapping

Both drivers **discover** the device-side memory windows (PCI BARs
or DT `reg` ranges) but **do not** map them. The actual mapping
request is routed through the driver host's memory capability by the
upper driver (Stage 4.D virtio-blk / virtio-net). This is the
direct consequence of `AGENTS.md` §4 ("memory isolation is enforced
by hardware") — the bus driver is not in the trust path for memory
mapping.

## PCI configuration-access library — `lib/pci`

### Configuration access

The enumeration, capability-walk, BAR-sizing, and window/MSI-X
hand-off core is parameterised over the `ConfigSpace` trait, so it is
independent of how configuration space is reached. Two access
mechanisms implement that trait; the caller picks one at construction
(`mechanism_one` / `mechanism_ecam` / `mechanism_brcm`).

#### Mechanism #1 — legacy I/O ports (x86_64)

PCI Local Bus 3.0 §3.2.2.3.2:

| Port  | Purpose                       |
| ----- | ----------------------------- |
| `0xCF8` | 32-bit configuration address |
| `0xCFC` | 32-bit configuration data    |

The crate splits the `in`/`out` instructions behind the
`rustos_abi::PortIo` seam so the in-crate unit tests can exercise the
bridge against a recording mock without touching real I/O ports; the
only real `PortIo` implementation lives in the x86_64 architecture
port. `ConfigAddress::to_cf8` is the single defensive gate — an
out-of-range address reads the `0xFFFF_FFFF` "no device" sentinel
rather than reaching a port.

#### ECAM — memory-mapped PCIe configuration (cross-arch)

PCI Express Base 3.0 §7.2.2 maps configuration space flat into MMIO:
each `(bus, device, function)` owns a 4 KiB block, so a configuration
dword is a naturally-aligned access at the computed byte offset
(`ConfigAddress::ecam_offset`):

```text
 bits 27..20: bus       (one 1 MiB block per bus)
 bits 19..15: device    (one 32 KiB block per device)
 bits 14..12: function  (one  4 KiB block per function)
 bits 11..0 : register byte offset
```

`EcamConfigSpace` reads and writes through a kernel-mapped
`rustos_abi::RegisterWindow` over the host bridge's configuration
region — obtained from the MMIO-map facility after a `CAP_MMIO_MAP`
check, so the driver never synthesises a pointer (`AGENTS.md` §4).
An access past the window's length, or a malformed address, resolves
to the same `0xFFFF_FFFF` sentinel, so an enumeration walk that runs
off the mapped buses fails closed rather than reading out of bounds
(`AGENTS.md` §5.4). Flat ECAM is the path any PCIe host bridge with a
contiguous MMCONFIG region uses; it carries no target-conditional
`cfg` (`AGENTS.md` §17.2).

#### BCM2711 windowed configuration access (Raspberry Pi 4)

The Raspberry Pi 4's BCM2711 root complex does **not** map
configuration space flat. Its own root-bus header (`bus 0`, `devfn 0`)
is read directly at the controller base, but a downstream function
(`bus >= 1`, e.g. the VL805 xHCI at `01:00.0`) is reached through an
index/data window pair inside the controller's own register block:
the function's `(bus << 20) | (devfn << 12)` block address is written
to the `EXT_CFG_INDEX` register (`0x9000`), then the dword is accessed
through the 4 KiB `EXT_CFG_DATA` window (`0x8000`) at the register
byte offset. `BrcmConfigSpace` implements `ConfigSpace` with exactly
this windowing — the *only* BCM2711-specific knowledge; the
enumeration, BAR-sizing, and capability walk above it are unchanged.
`mechanism_brcm(window, secondary_bus)` builds the bus over it. An
access that lands outside the mapped window, or any function but `00.0`
on the root bus, resolves to the same `0xFFFF_FFFF` sentinel
(`AGENTS.md` §5.4). The link behind the bridge must be **up** before any
downstream access — the `drivers/bus/pcie_brcm` root-complex bring-up
(below) guarantees that before handing its register window here.

The BCM2711 root port is a **single-device** link, so the accessor
forwards a configuration transaction only to `device 0` on
`secondary_bus` (the bus number the bring-up programmed into the bridge
bus-number register) and resolves every other downstream target to the
`0xFFFF_FFFF` sentinel *without* issuing a transaction. This is not just
hygiene: once the root port forwards downstream, a config read to a
non-existent target forwards a TLP that nothing answers, and the
completion timeout becomes a CPU external abort — a flat 256-bus walk
over forwarded config would wedge the boot CPU. The gate resolves a
non-zero slot on a non-root bus to the sentinel without forwarding.

### Enumeration walk

- 256 buses × 32 devices × 8 functions.
- Function 0 must be present before higher functions of the same
  device are probed; the multifunction bit is checked at offset
  0x0C, bit 7.
- `vendor == 0xFFFF` is the "slot empty" sentinel and is skipped.

### Capability list walk

Triggered when status bit 4 is set; the walker follows the linked
list rooted at offset 0x34 with a hard upper bound of 64 entries to
defend against circular `next` pointers (returns
`DriverError::DeviceFault`). MSI (`cap_id = 0x05`) and MSI-X
(`cap_id = 0x11`) are decoded structurally; every other capability
ID is reported opaquely so the host can audit it without re-walking
configuration space.

### BAR walker

Reads each BAR slot of a type-0 header, advances by two slots for
64-bit memory BARs, and runs the standard FFFFFFFF/read-back/restore
probe to compute the window size. Non-type-0 headers
(PCI-to-PCI bridge, CardBus) yield `DriverError::Unsupported`; they
are out of scope for Stage 4.

### Acceptance: exact q35 device list

`tests::q35_enumeration_matches_exact_device_list` asserts the
following list against a mock-host fixture reproducing QEMU's `q35`
default PCI tree:

| BDF      | vendor:device | class  | role              |
| -------- | ------------- | ------ | ----------------- |
| 00:00.0  | 8086:29C0     | 0x0600 | Host bridge       |
| 00:03.0  | 1AF4:1041     | 0x0200 | virtio-net-pci    |
| 00:1f.0  | 8086:2918     | 0x0601 | LPC bridge (mf)   |
| 00:1f.2  | 8086:2922     | 0x0106 | AHCI SATA         |
| 00:1f.3  | 8086:2930     | 0x0C05 | SMBus             |

The same enumeration core is exercised by the `Bus::enumerate`
implementation that the driver host wires up after `register`.

### Acceptance: VL805 over ECAM

`tests::ecam_enumeration_finds_root_port_and_vl805` and
`tests::ecam_capability_walk_decodes_vl805_msix` lay a flat ECAM
region (a PCIe root-port bridge at 00:00.0 and the Pi 4's VL805 xHCI
`1106:3483` at 01:00.0, with absent slots reading the all-ones
sentinel as real hardware master-aborts) and drive the same
enumeration and capability-walk core over `EcamConfigSpace`,
asserting both devices are listed and the VL805's MSI-X capability
decodes. The BAR *size* probe depends on hardware read-only BAR bits
and is covered by the mechanism-#1 fixtures, not the plain-memory
ECAM backing.

## BCM2711 PCIe root-complex bring-up — `drivers/bus/pcie_brcm`

The Pi 4's VL805 xHCI sits behind the BCM2711 PCIe root complex, which
ships out of reset with its link **down**. Before the windowed
configuration access above can reach the VL805, the root complex must
be brought up. The `drivers/bus/pcie_brcm` driver crate's `lib` target
performs that bring-up over the BCM2711 root-complex registers. (The
bring-up engine is co-located in that driver crate, not a `lib/*`
device-support crate: PCIe root-complex bring-up sits above the §18.6
bootstrap floor, so it has no charter-legal non-driver consumer for the
§2.20 carve-out, `AGENTS.md` §2.22.)

### Seams

The `BrcmPcieRc` state machine is written against two seams so it is
proven host-side (`AGENTS.md` §2.2):

- `PcieRegs` — controller register access, implemented for the
  kernel-minted `RegisterWindow` on metal and a register-level mock in
  tests (the `emmc2` `SdhciHost` shape).
- `Delay` — a microsecond busy-delay for the bring-up's hard timing
  requirements (SerDes settle, the 100 ms post-`PERST#` link-training
  window), supplied by the kernel composition on metal and a no-op in
  tests.

### Bring-up sequence

Release the controller's bridge reset **before** touching any MISC
register: the BCM2711 holds the controller core off at OS entry, and a
MISC-block (`0x4xxx`) access does not complete until the always-accessible
RGR1 bridge `sw_init` reset (`0x9210`) is released — touching MISC first
master-aborts on the SoC bus completion timeout (~10.8 s on metal),
which is what the multi-second bring-up pause turned out to be. So,
following the BCM2711 PCIe bring-up sequence: release the bridge `sw_init`
reset, bringing the core and its MISC block online, then let it settle. This is
the gentlest **no-touch-probe** bring-up: the previous boot stage
(`start4.elf`) hands off with the bridge `sw_init` reset **and** `PERST#`
already asserted and the VL805 firmware loaded over the power-on link, so
the driver does **not** re-assert a fundamental reset or toggle the SerDes
`IDDQ` — either of which could drop that resident firmware — and the
link-up step deasserts the already-asserted `PERST#`, producing the single
deassert edge. On the Pi 4 the VL805's xHCI firmware is loaded by the
bootloader EEPROM (via VideoCore), and on such a board VideoCore (re)loads
the blob on the **`PERST#` deassert edge** — the only edge the bring-up
drives, after which no runtime VL805 reload is issued.
So this driver produces that single deassert edge (rather than a fresh
fundamental reset), and the keyboard composition deliberately does **not**
issue a runtime `NOTIFY_XHCI_RESET` reload if the VL805's firmware version
(config `0x50`) stays `0` after the link trains — issuing a redundant reload
can be destructive on Pi firmware. Then program
`MISC_CTRL` (SCB access, UR config reads, 128-byte burst, RCB modes);
program the inbound (PCIe→system-memory) viewport `RC_BAR2` from the
discovered `dma-ranges` (the size encoded by `encode_ibar_size`, the
size rounded up to a power of two); disable the unused `RC_BAR1` /
`RC_BAR3` inbound windows; confirm the root-port role (fail closed
with `DeviceFault` otherwise); advertise ASPM L0s+L1 and present the
root complex as a PCI-PCI bridge; program the bridge bus-number register
(primary 0, secondary/subordinate = the single downstream bus) so the
port forwards configuration to the directly-attached VL805; program the
bridge Memory Base/Limit window (config offset `0x20`, covering the
outbound PCIe range) so the port forwards *memory* transactions to the
VL805's BAR — the BCM2711 ships that register empty, so without it BAR
reads master-abort to the `0xdead_dead` poison even though config reads
succeed (the bridge-window assignment a full PCI enumerator performs, which
the windowed `mech_brcm` accessor does not); program the outbound (CPU→PCIe)
MMIO window from the discovered `ranges`. Finally deassert `PERST#` and
poll `MISC_PCIE_STATUS` for data-link-active + phy-link-up, bounded by
`DEFAULT_LINK_POLLS` (100 ms), and confirm the link with a fail-closed
`link_up()` (`DeviceFault` otherwise). **Only then** enable Memory Space
+ Bus Master in the bridge's *own* Command register (config offset
`0x04`) — the standard PCI-PCI bridge enable a full enumerator performs,
which the windowed `mech_brcm` accessor
does not. This is issued *after* the link is up, because a PCI-PCI bridge
is enabled only once the link trains: the integrated RC latches
Memory Space Enable against a live link, so an earlier write (with
`PERST#` still asserted) does not stick — the metal `4110` symptom that
read the bridge command back as `0x0000` and left the VL805 BAR
master-aborting to `0xdead_dead`. All windows are device-tree-discovered, never compiled-in
(`AGENTS.md` §18.1).

`entry_inbound_window` exposes the inbound (PCIe→system-memory) viewport
registers (`RC_BAR1_LO`, `RC_BAR2_LO`/`HI`, `RC_BAR3_LO`) **as the previous
boot stage left them**, captured read-only and fail-closed during `bring_up`
before `RC_BAR2` is reprogrammed. On the Pi 4 the boot firmware's VL805
handoff depends on that inbound DMA window, so the capture both drives the
"don't reprogram a firmware-configured window" decision in `bring_up` and
lets a metal run compare it with the known-good
`IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000` (`AGENTS.md` §15.7). The
post-bring-up window read-backs that once logged the trained register block
were removed: on real BCM2711 silicon reading those MISC registers after the
link trains stalls for seconds while the bring-up holds the CPU,
and with the link confirmed up they added no functional value
(`AGENTS.md` §2.14 / §2.16).

### Composition

The crate owns its discovered-node parsing and its autonomous floor
entry, beside the link-training engine they feed (`AGENTS.md` §2.2 /
§2.21): `wiring::pcie_bringup_from_node` reads the controller register
window plus the inbound/outbound address windows off the discovered
`brcm,bcm2711-pcie` `HwNode` into a `PcieBringup` (failing closed with a
`BringupError` naming the first missing resource — never an invented
window, `AGENTS.md` §18.5), and `wiring::bring_up_from_node` is the §18.6
autonomous bootstrap-floor entry that maps the window under `CAP_MMIO_MAP`
and trains the link over it (`DriverError::NotFound` on an incomplete
node). `wiring::open_discovered` is the lower seam they share with a
caller that already holds the windows. The caller then recovers the
window (`into_regs`) and builds `mechanism_brcm(window)` to enumerate the
VL805. The crate performs only the PCIe link bring-up and so never depends
on another driver crate (`AGENTS.md` §17.4); the VL805 firmware reload is
the separate `drivers/bus/usb/vl805` device crate's job and the xHCI
bring-up the separate `drivers/bus/usb/xhci` crate's. Both windows the
`PcieWindows` carries are device-tree-discovered: the inbound aperture
from the node's `dma-ranges` (an `HwResource::dma_translated` carrying
the CPU-reachability top, extent, and the inbound PCIe-space base) and
the outbound MMIO window from its `ranges` (an `HwResource::bus_window`
carrying the CPU base, size, and far-side PCIe base —
`kernel/arch/aarch64::fdt::{dma_ranges_aperture,outbound_mmio_window}`,
`AGENTS.md` §18.1).

The whole chain runs in **user space**, decoupled by the hardware tree —
no driver names another (`AGENTS.md` §17.4 / §4). The kernel boot walk
seeds the discovered `brcm,bcm2711-pcie` root complex and VideoCore mailbox
nodes, and `devmgr` autoloads each signed `/System/Drivers/` bundle against
its node: the `pcie_brcm` bus driver maps its register window and trains the
link (`open_discovered`), assigns the VL805 BAR, and publishes the VL805 PCI
function through `hw_emit_node` (carrying the BAR + DMA grants); the `vl805`
device driver binds that, reloads the controller firmware over the VideoCore
mailbox, and publishes the controller as a `usb,xhci` node forwarding those
grants; and the `usb_kbd` driver binds *that*, maps the BAR, carves DMA,
brings the controller up (`usb::wiring`), enumerates the boot keyboard, and
pumps decoded key edges into the input-focus arbiter through `key_inject`.
Each driver receives only the grants its matched node requested (`AGENTS.md`
§18.3), reached through its rt-backed `DriverHost`. The engines are
host-tested up to the controller hand-off, where the inert mock register
window faults — the metal boundary; QEMU models no Pi PCIe link timing or
USB, so the live enumerate→emit→autoload chain is the metal acceptance item
(`plans/PI.md` P10 D5d, `AGENTS.md` §0.9).

## MMIO driver — `drivers/bus/mmio`

### DTB iterator

The boot DTB is parsed once through `rustos_fdt::Fdt`. This is the
single shared device-tree parser in the workspace (`AGENTS.md`
§2.2): the architecture ports' platform discovery, the QEMU
verticals, and this driver all walk the `virt` tree through it. It
validates the FDT header, bounds-checks every read, and never
panics. The MMIO driver walks every node with `Fdt::nodes`, filters
on `compatible = "virtio,mmio"` (`Node::is_compatible`), reads
`reg = <base length>` (`Node::property` + `Property::read_be_u64`),
then probes the four-register identifier window through the volatile
reader.

### Volatile read seam

The only `unsafe` block in the crate sits inside
`VolatileMmioRead::read32` and is bounds-checked against the
`base_phys + len` window the constructor recorded. The trait
(`MmioRead`) is the in-crate test substitution point.

### Acceptance: exact virt slot list

`tests::virt_enumeration_matches_exact_device_list` asserts the
following list against a four-slot `virt`-style DTB plus a fake
register window in which two slots are populated:

| Slot base    | DeviceID | role           |
| ------------ | -------- | -------------- |
| 0x0A00_0000  | 1        | virtio-net     |
| 0x0A00_0200  | 2        | virtio-blk     |

The two trailing slots have `DeviceID == 0` and are skipped — the
same behaviour `virtio-mmio.c` in QEMU exhibits for unattached
transports.

## xHCI driver — `drivers/bus/usb/xhci`

The Pi 4 reaches its USB-A ports through a VL805 PCIe xHCI controller
(`plans/PI.md` P10). The bus-agnostic xHCI protocol layers and the
multi-device enumeration engine live in the `lib/usb`
(`rustos-usb`) crate — the USB analogue of `lib/virtio` — so this driver
and an arch-neutral user-space keyboard driver can both build on the same
engine without depending on each other (`AGENTS.md` §17.4). This driver
crate adds the §8 `register` entry, the §18.3 `BIND_KEYS` bind table, and
the PCI BAR / hwtree `wiring` over that protocol; the live controller
bring-up for the VL805 is the remaining P10 metal increment, and QEMU
models no Pi USB timing, so the host suite is the emulation artefact and
metal acceptance stays a checklist. The protocol behaviour described
below is implemented in `lib/usb` (see `docs/src/lib/usb.md`).

### Register seam and bring-up

Every controller access goes through the crate's `XhciHost` seam —
implemented for the kernel-minted `RegisterWindow` on metal, and for a
register-level mock in tests (the `emmc2` `SdhciHost` shape, `AGENTS.md`
§2.2). `Xhci::open` validates the capability block
(`CAPLENGTH`/`HCIVERSION` plausibility, non-zero
`MaxSlots`/`MaxPorts`/`DBOFF`/`RTSOFF` — the absent-controller
all-ones read fails here), halts a running controller, then issues the
self-clearing Host Controller Reset. Before asserting `HCRST`, it clears
only the stale write-1-to-clear `USBSTS` latches RustOS has observed on
firmware handoff (`HSE|PCD`); `Controller Not Ready` is enforced after
that reset, not treated as an unrecoverable pre-reset state. A halted
controller handed over with stale `CNR|HSE|PCD` may need those latches
cleared before the reset completes, while a post-reset `CNR` still fails
closed. Every wait is poll-budget-bounded and fails closed with
`DeviceFault` (`AGENTS.md` §2.1); the controller is left halted.
The same logic is exposed through `Xhci::open_diagnostic`, which keeps
the fail-closed `DriverError` but also names the refused stage
(`capability`, `halted_before_reset`, `reset_self_clear`, or
`controller_ready_after_reset`) and carries the last readable
`USBCMD`/`USBSTS` snapshot. The Pi keyboard bring-up logs those fields on a
metal `4101` open failure so the next capture identifies the exact stuck
reset condition instead of collapsing every timeout to a bare `device_fault`.
`Xhci::start` then programs the DMA structures and runs it: `CONFIG`
(all reported slots enabled), `DCBAAP`, `CRCR` (consumer cycle state
1), interrupter 0's single-entry event ring segment table over
`RTSOFF` (`ERSTSZ`/`ERSTBA`/`ERDP`), and Run/Stop — refusing any
address that is zero or not 64-byte aligned (`DmaProgram` plausibility,
§6.1, fail closed). `Xhci::ack_event` advances `ERDP` (clearing Event
Handler Busy) after each consumed event. `PORTSC` reads decode through
`PortStatus` with 1-based port bounds checks, `Xhci::reset_port` runs
the §4.19.5 port reset with the write-1-to-clear bits masked so no
pending change bit is consumed by accident, and doorbell rings
validate both the index (≤ `MaxSlots`) and the §5.6 target rules.

### TRB rings

`trb` defines the 16-byte TRB plus fail-closed `TrbType` /
`CompletionCode` subsets (an unknown type or completion code is
`OutOfRange`, never a guess), the on-ring little-endian byte
conversion, and the transfer-event field decoders (slot ID, endpoint
ID, transfer residual). `ring` carries the §4.9 state machines and
holds **no memory**: `ProducerRing::push` returns a `PushOutcome` —
the cycle-stamped TRB, its slot and device-visible address, and (on a
wrap) the re-cycled Link TRB to publish *after* the data TRB — so the
owner of the device-shared memory performs every write and the
cycle/wrap/full logic is host-proven. The ring refuses caller-set
cycle bits and caller Link TRBs and fails closed (`Busy`) when full;
`EventRingCursor` consumes only TRBs whose cycle bit matches its
expectation, inverting it on each wrap, and holds no borrow of the
segment (the controller keeps writing it), validating the segment
length on every `pop`.

### Device enumeration and the HID report path

`device` is the multi-device enumeration engine. All device-shared
bytes live in one caller-provided region behind the crate's
`DmaRegion` seam — implemented for the `lib/abi` `DmaSlab` in
production and by a plain shared buffer in tests — and the engine
computes a 64-byte-aligned `Layout` inside it: the shared structures
(DCBAA, ERST, command ring, event segment, input context, the root
device's output context and EP0 ring, the hub status-change ring and
report, the control data buffer) plus one region per concurrently
served device (`MAX_DEVICES`, each holding an output context, EP0 /
interrupt-IN / bulk transfer rings, report buffers, and bulk staging),
refusing a region that is misaligned or too small.

`UsbDevice::start` zeroes the region, publishes the ERST entry and
the rings' Link TRBs, and starts the controller through `Xhci::start`.
`UsbDevice::enumerate_hid(port)` then brings the device on a root-hub
port to the configured state (§4.3): port reset when the port is not
yet enabled, Enable Slot (validating the returned slot ID), Address
Device (input control context `A0 | A1`, slot context, EP0 context
with the speed-derived **worst-case** max packet size), an 8-byte
`GET_DESCRIPTOR(device)` prefix read ending at `bMaxPacketSize0` —
one packet at the smallest legal EP0 size, so it completes whatever
size the device really uses — whose validated value (low speed 8,
full speed 8/16/32/64, high speed 64, SuperSpeed exponent 9; anything
else is `BadMagic`) drives an **Evaluate Context** (§4.6.7) whenever
it differs from the assumed size (a full-speed wireless receiver's
8-byte EP0 otherwise terminates every longer EP0 IN read short at its
first packet — the metal `DeviceFault`), then the full 18-byte
`GET_DESCRIPTOR(device)` (decoded fail-closed — a forged length, type,
or zero-configuration descriptor is `BadMagic`),
`GET_DESCRIPTOR(configuration)` in two steps (the 9-byte header for
`wTotalLength`, then exactly that many bytes — never an over-long
request a buggy device might mishandle; the interface descriptors'
class triples and `bConfigurationValue` / `bInterfaceNumber` drive the
steps below — never assumed), and `SET_CONFIGURATION`.

The interrupt-IN endpoint is configured (Configure Endpoint) and
`SET_PROTOCOL(boot)` is issued **only for a HID interface**. It is not
primed during enumeration: `next_report` arms one transfer only when the
class driver has submitted a URB and is waiting for that report. A hub
reports interface class `0x09`, not HID: it keeps only its control endpoint,
because this engine reads a hub's downstream ports over EP0 hub-class
`GET_STATUS`, never its interrupt status-change endpoint. Arming that
endpoint for a hub would make the hub deliver asynchronous status-change
reports that interleave with — and fault — those EP0 control transfers (a
transfer event whose interrupt-TRB pointer is not in the control wait's watch
list → `REJECT_ADDRESS_MISMATCH`, then a wedged ring; the metal symptom was
the hub's per-port `GET_STATUS` reads returning the all-ones sentinel).

Devices *downstream* of an enumerated hub (every external device on the
Pi 4B hangs off the onboard `2109:3431` VIA hub) are each reached on
their **own xHCI slot** — the `bring_up` walk attaches every connected
downstream port, up to `MAX_DEVICES` devices at once, so a keyboard and
a storage stick are served together. `UsbDevice::attach_downstream_device
(down_port, speed)` keeps the hub addressed on its slot and gives the
new device the EP0 ring and output context of a free device region
(`control`/`address_device`/`next_report` follow the active slot through
`ep0_ring_off`/`output_ctx_off`), then Enable Slot + Address Device
with a slot context carrying the **Route String** (the hub's downstream
port, §8.9) and — for a full/low-speed device behind the high-speed hub
— the **transaction-translator** Hub Slot ID and Port Number (§6.2.2),
so the controller splits its transactions through the hub's TT. The
post-Address sequence (descriptors → Configure Endpoint →
`SET_CONFIGURATION` → `SET_PROTOCOL(boot)` → ready for request-driven report
arming) is the shared `finish_enumeration`, identical to a root-port device —
only the topology in the slot context differs. A failed attach restores
the hub as the active control context and releases the claimed slot, so
one port's broken device never costs the other ports their service. The
caller owns the wall-clock power-on-good and reset-recovery delays: it
powers the port, resets it (`SET_FEATURE(PORT_RESET)`), waits, and
confirms the port enabled with the speed read from `GET_STATUS` before
addressing.

Before addressing anything behind it, the bring-up walk first
**marks the hub as a hub** in its own slot context
(`configure_hub_slot`): it reads the hub descriptor (`bNbrPorts` and the
`wHubCharacteristics` TT Think Time, `read_hub_topology`), copies the
controller's live output slot context, sets the **Hub** bit, **Number of
Ports**, and **TT Think Time** (single-TT, so the Multi-TT bit stays
clear), and issues a Configure Endpoint over the hub's slot that names
only the slot context (Add flag `A0`). Without this the controller never
schedules the split transactions a full/low-speed device behind the hub
needs, so the keyboard is addressed (Address Device succeeds) but its
interrupt-IN endpoint never completes and it delivers no report — the
metal symptom where the keyboard enumerated but typing produced nothing
(xHCI §6.2.2).

The interrupt-IN endpoint context also carries a non-zero **Max ESIT
Payload** (`ep_ctx_dwords`, §6.2.3.8 dword 4 bits 16:31 = the max packet
size for a boot HID endpoint). The xHCI periodic scheduler reserves no
bus bandwidth for a periodic endpoint whose Max ESIT Payload is zero
(§4.14.2), so the controller would service it never — fatal precisely
for a full/low-speed interrupt endpoint behind the hub's TT, where the
scheduler must budget the split transactions. With the hub marked but
the payload left zero, Address Device and Configure Endpoint both
succeed, yet the keyboard delivers no report and the poll loop spins
with zero events — the metal symptom where the addressed keyboard never
typed. A control endpoint (Interval `0`) leaves the field reserved-zero.

The interrupt-IN endpoint itself is **read from the configuration
descriptor, never assumed** (`InterfaceInfo::decode_all`). The driver
walks past each default-alternate interface descriptor to its first
interrupt-IN endpoint and takes its Device Context Index
(`2 × endpoint_number + 1`), `wMaxPacketSize`, and `bInterval`;
`finish_enumeration` then configures *that* DCI per served interface, and
`next_report` doorbells and drains it for each waiting URB.
`interrupt_interval` encodes the endpoint-context Interval from the
descriptor's `bInterval` and the device speed (high/SuperSpeed
`bInterval − 1`; full/low-speed frames → the `fls(bInterval × 8) − 1`
microframe exponent, clamped 3..=10, xHCI Table 6-12). Hard-coding the
endpoint as endpoint 1 (DCI 3) left the controller polling — and the
doorbell ringing — the wrong endpoint for a keyboard whose interrupt-IN
endpoint sat elsewhere, so it scheduled the real endpoint never: the
keyboard was addressed (`4128`) with the hub marked and a non-zero Max
ESIT Payload, yet typing produced nothing and the poll loop spun with
zero events. A HID interface that reports no interrupt-IN endpoint is a
forged/corrupt descriptor and fails closed (`BadMagic`, §2.9).

Control transfers carry the SETUP payload as immediate data, set
Interrupt-on-Short-Packet on the IN data stage, and watch only the
addresses of their own in-flight TRBs: a completion for a TRB never
issued, an undecodable completion code, an unexpected event type, or a
stalled request is a `DeviceFault`, and every wait is bounded by the
engine's poll budget (`AGENTS.md` §2.1 / §2.9).

`UsbDevice` implements the `rustos_abi::driver::input::ReportSource`
seam (hoisted into `lib/abi` because its consumer,
`drivers/input/usb_kbd` (and its mouse sibling), is a sibling driver and drivers depend only
on `lib/*`, `AGENTS.md` §17.4): when no interrupt-IN transfer is in flight,
`next_report` arms exactly one TRB for the class-driver URB currently waiting
and rings the endpoint doorbell, returning `None` so the HCD holds the IPC
ticket. When the controller event arrives, the next `next_report` consumes
that event, validates the controller's claim end to end (slot, endpoint ID,
completion code, TRB address inside the interrupt ring, residual within the
TRB length — §5.4), copies the report out of the slot's buffer, and retires
the transfer. The crate's tests prove the whole chain against the
register-level mock plus an in-memory ring model sharing the same
buffer — including a `BootKeyboard` polling decoded key events over
the mock controller — plus the fail-closed paths (forged residual,
stalled class request, empty port, double enumeration, undersized or
misaligned DMA region).

## VL805 USB bus driver — `drivers/bus/usb/vl805`

The VL805 firmware (re)load is the one thing specific to that *device*,
so it is its own driver — separate from, and not intertwined with, the
generic PCIe root-complex driver (`drivers/bus/pcie_brcm`, which trains the
link) and the generic xHCI host engine (`lib/usb`, which brings the
controller up and enumerates devices). A different board may need the PCIe
driver without USB at all, or an xHCI controller that needs no firmware
reload; keeping the three separate is the correct modular shape
(`AGENTS.md` §2.2 / §8 / §17.4).

The firmware policy and the controller-node wiring live **in the driver
crate** `drivers/bus/usb/vl805`, as a host-testable `lib` target
(`src/lib.rs` + `src/wiring.rs`) that the crate's freestanding `Run` binary
(`src/main.rs`, which links the userland runtime `rustos-rt`) links. The
logic is co-located here, not in a `lib/*` device-support crate: a VL805
USB driver sits above the §18.6 bootstrap floor and so has no charter-legal
non-driver consumer for the §2.20 carve-out (`AGENTS.md` §2.22). Putting it
in a `lib` target keeps it host-unit-tested without a kernel and the binary
crosses no `drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4 / §2.2).

On a Pi 4 without the SPI EEPROM (rev 1.4+), the VL805 carries no
resident firmware: the `VideoCore` loads it at power-on and a PCIe
`PERST#` drops it, so only `VideoCore` can reload it over a
`NOTIFY_XHCI_RESET` firmware-property request. The driver may know the
VL805/BCM2711 — but it reaches the firmware mailbox **only** through the
board-neutral `rustos_abi::driver::mailbox::MailboxChannel` seam, never a
doorbell address or a `kernel/*` dependency (`AGENTS.md` §17.4). Its
public surface is the §8 `register` entry, the §18.3 `BIND_KEYS` bind
table (exact PCI `1106:3483`, ranked above the generic class-wildcard
xHCI driver), two firmware-policy functions composed over a
`MailboxChannel` —

- `probe_firmware_revision` — a benign firmware-revision liveness read
  that separates a broken mailbox path from `VideoCore` dropping the
  reset tag (`AGENTS.md` §15.7), and
- `reload_firmware` — the `NOTIFY_XHCI_RESET` reload, fail-closed: an
  unverified firmware ack is treated as a failure, never a success
  (`AGENTS.md` §5.4) —

and the `wiring` composition the bin runs: `build_xhci_node` publishes
the controller as `node B`, an `usb,xhci` hardware-tree node (the shared
`rustos_usb::XHCI_COMPATIBLE` identity) **forwarding** the register BAR +
DMA grants the bin received on the VL805 PCI node (`node A`), and
`reload_firmware_and_publish` reloads the firmware then emits node B —
so firmware-before-bring-up holds by construction (node B does not exist
until the reload runs). The bin holds only `CAP_MAILBOX` + `CAP_HW_EMIT`:
it forwards the BAR/DMA grants without ever mapping them (`AGENTS.md` §4
— least privilege), and `drivers/input/usb_kbd` binds node B
(`rustos_hid::KEYBOARD_BIND_KEYS`) to bring the controller up.

The property-message *layout* lives once in `lib/vcmailbox`
(`encode_xhci_reset` / `decode_xhci_reset_response` and the
firmware-revision pair); the VL805 driver only sequences the policy, never
re-deriving the layout (`AGENTS.md` §2.2). The mailbox *mechanism* (the
discovered doorbell window, the DMA-aliased property buffer, the cache
coherency) lives behind the `MailboxChannel`: the user-space bin reaches
the autoloaded `drivers/bus/mailbox/vcmailbox` service over the kernel
call surface (the `ipc_call` endpoint, gated by `CAP_MAILBOX`). QEMU
models no `VideoCore`, so
the policy is host-proven (in the driver crate's `lib` target) against the
protocol-faithful `lib/vcmailbox` mock firmware and the reload-and-publish
wiring against `DriverHost`
doubles; the live reload → publish chain is the on-metal acceptance item
(`plans/PI.md` P10).

## Register-window hand-off

Enumeration only *names* a device; before a virtio transport can
drive it, the device's register block has to be mapped into the
driver's address space. A bus driver never synthesises that pointer
itself — doing so would be ambient authority, which `AGENTS.md` §4
forbids. Instead the kernel is the sole minter of a register window.

### The seam

`lib/abi` defines two types and one trait:

- `RegisterWindow` — a capability-checked, kernel-mapped MMIO window.
  Its only constructor (`from_mapping`) is `unsafe` and is called
  *only* by the kernel after it has validated the mapping, so safe
  code can never fabricate one. Every accessor (`read_u32` /
  `write_u32` / …) is bounds- and alignment-checked and returns
  `WindowError` rather than touching memory out of range.
- `MmioMapper` — the kernel-side MMIO-map facility the bus driver
  calls: `map_window(phys_base, len) -> Result<RegisterWindow,
  MmioMapError>`.
- `MmioMapError` — `CapabilityMissing` / `InvalidRegion` /
  `Unsupported`, each with an `as_driver_error()` mapping.

The host hands the bus driver an `&dyn MmioMapper` through
`DriverHost::mmio_mapper()` (default `None`). The kernel's concrete
mapper is `KernelMmioMapper` in `kernel/virtio` (the kernel crate,
because it links `kernel/{mem,sec}`, which a driver may not —
`AGENTS.md` §17.4); it wraps `kernel/mem::MmioMap` and routes every
request through the capability gate `kernel/sec::map_mmio`.

### Capability flow

```text
bus driver                     kernel (KernelMmioMapper)
----------                     --------------------------
resolve (phys_base, len)
   PCI : Pci::map_bar_window(bdf, bar_index, mapper)
   MMIO: Mmio::map_slot_window(base, mapper)
        │
        └── mapper.map_window(phys_base, len) ──► kernel/sec::map_mmio
                                                    1. check CAP_MMIO_MAP
                                                       │ no  → MmioMapDenied (audit 1041)
                                                       │       Err(CapabilityMissing)
                                                       │ yes
                                                    2. MmioMap::map  (NO_CACHE, guard pages)
                                                    3. emit MmioMapped (audit 1040)
        ◄────────────── RegisterWindow ─────────────┘
   wrap in PciBackend / MmioBackend (virtio transport)
```

The kernel maps the device's *own* physical frames with caching
disabled (`MapFlags::NO_CACHE`) and brackets the window with guard
pages, so a driver that walks off the end of a register block faults
instead of poking a neighbouring device (`AGENTS.md` §4). The grant
and every refusal are recorded in the audit log (events `1040`
`MmioMapped` / `1041` `MmioMapDenied`; see
`architecture/security.md`).

The PCI hand-off resolves the requested memory BAR (refusing I/O-port
BARs, which are reached through port I/O, and unused BARs); the MMIO
hand-off reads the `<base, length>` pair from the matching
`virtio,mmio` device-tree node. Neither path can run without the
caller holding `CAP_MMIO_MAP`.

### virtio-1.x configuration windows

A modern virtio-PCI device does not expose its register blocks as
whole BARs; instead it publishes each configuration structure as a
vendor-specific capability (`cap_id = 0x09`) carrying a
`(cfg_type, bar, offset, length)` tuple (virtio 1.x §4.1.4). The PCI
capability walker decodes these into `Capability::Virtio` /
`Capability::VirtioNotify` records, and
`Pci::map_virtio_window(bdf, cfg_type, mapper)` resolves a requested
`cfg_type` — common (`1`), notify (`2`), ISR (`3`), or device (`4`) —
to its `bar.base + offset` physical address and maps exactly `length`
bytes through the same `CAP_MMIO_MAP`-gated `MmioMapper`. The
`bar_offset + length` span is bounds-checked against the resolved BAR
size before the mapping request, so a malformed capability fails
closed (`OutOfRange`) rather than mapping past the device's window.
`Pci::virtio_notify_off_multiplier(bdf)` returns the notification
scale from the notify capability. The four windows plus the
multiplier are exactly what `PciTransport::new` consumes, so a
boot-time PCI walk hands a working modern-virtio transport to the
driver host without ever synthesising a pointer.

### Ring-0 virtio-PCI walk

These hand-offs are `pub(crate)` on the concrete `Pci` type, because a
driver crate's only public surface is `register` (`AGENTS.md` §8). Ring
0 therefore reaches them through a frozen ABI seam rather than the
concrete type: `Pci<C>` implements
`rustos_abi::driver::virtio_pci::VirtioPciBus` (a supertrait of `Bus`),
whose `map_virtio_window` / `notify_off_multiplier` methods forward to
the inherent ones. The kernel's `provision_virtio_pci(bus, device_id,
mapper, build)` (in `kernel/virtio/src/virtio_pci_walk.rs`) takes a
`&dyn VirtioPciBus`, enumerates the bus into a bounded table, picks the
first function matching `VIRTIO_PCI_VENDOR_ID` and the requested device
ID, maps the four windows through the `CAP_MMIO_MAP`-gated `MmioMapper`,
reads the notify multiplier, and assembles a `PciTransportWindows`
(which lives in `lib/virtio`). It does not name a concrete transport
itself: the caller passes `build` — in production
`PciTransport::new` — so `kernel/virtio` depends only on `lib/*` and
never on the `drivers/bus/virtio` crate (`AGENTS.md` §17.4:
`kernel/* → lib/*`, never a driver). Ring 0 thus names no concrete
`drivers/bus/*` type and holds no ambient authority — the capability
check lives in the mapper, and every failure is a typed
`VirtioPciWalkError` rather than a panic (`AGENTS.md` §2.9).
The constants live once in `rustos_abi`; the driver's `VIRTIO_CFG_*`
names bind to them rather than re-stating the literals (`AGENTS.md`
§2.2).

### Ring-0 virtio-MMIO walk

The `virt`-platform path mirrors the PCI walk one level down. A
virtio-MMIO device is a single register block whose `<base, length>`
pair the MMIO bus driver reads from its `virtio,mmio` device-tree node;
`Mmio::map_slot_window(base, mapper)` maps exactly that block through
the `CAP_MMIO_MAP`-gated `MmioMapper`. As with PCI, this hand-off is
`pub(crate)` on the concrete `Mmio` type, so ring 0 reaches it through
a frozen ABI seam: `Mmio<'_, T>` implements
`rustos_abi::driver::virtio_mmio::VirtioMmioBus` (a supertrait of
`Bus`), whose `map_slot_window` forwards to the inherent one. The
kernel's `provision_virtio_mmio(bus, device_id, mapper, build)` (in
`kernel/virtio/src/virtio_mmio_walk.rs`) takes a `&dyn
VirtioMmioBus`, enumerates the bus into a bounded table, picks the
first slot whose `DeviceID` matches the requested virtio device type
(the bare type over MMIO, not the PCI `0x1040 + type` encoding), maps
its single window, and hands it to `build` (in production
`MmioTransport::new`). As with the PCI walk, `kernel/virtio` names no
concrete transport type, so it depends only on `lib/*` and never on the
`drivers/bus/virtio` crate (`AGENTS.md` §17.4). Ring 0 holds no ambient
authority; every failure is a typed `VirtioMmioWalkError` rather than a
panic (`AGENTS.md` §2.9).

### Boot wiring

`provision_virtio_pci` yields the transport its `build` closure
constructs, but a virtio-class driver also needs a per-process DMA host
and a driver host to run its signed
`.rxe`. `kernel/rustos-kernel/src/virtio_boot.rs` joins the three:
`provision_and_run(config, make_table, body)` takes a
`VirtioBootConfig` bundling the bus (reached through both the
`VirtioPciBus` and `MsixBus` seams), the per-driver `MmioMap`, the DMA
frame allocator + direct physical map, the device's bound `IrqHandle`
plus the MSI-X table entry and architecture-built `MsiMessage` that
delivers its vector, and the driver-host trust inputs. It builds a
`KernelMmioMapper`, provisions the `PciTransport`, routes the device's
MSI-X interrupt through the same mapper (see below), constructs a
`KernelVirtioFactory`, and hands a live `drvhost::Host` (with the
factory wired into `HostConfig::virtio_host_factory`) plus the
transport to the `body` closure. The scope/callback shape keeps the
mapper, factory, and host — and every per-driver DMA pool the factory
mints — on one boot frame, so all of it is reclaimed when `body`
returns and no driver retains a register window or DMA mapping past its
load (`AGENTS.md` §4). The boot walk fails closed with a
`VirtioPciWalkError` and never constructs the host if the device, a
window, or the interrupt route cannot be resolved.

### MSI-X interrupt routing

Enumeration and window mapping bring a device's registers online; the
device also needs an interrupt line. A modern virtio-PCI function
delivers interrupts through MSI-X (PCI Local Bus 3.0 §6.8.2): one
message-signalled vector per table entry held in a memory BAR, plus a
per-function enable bit in configuration space. Routing the line means
programming an entry with the message the platform interrupt controller
minted, unmasking that entry, and enabling MSI-X on the function.

`Pci::route_msix(bdf, entry, message, mapper)` does exactly that: it
locates the function's MSI-X capability (decoded by the capability
walk), bounds-checks `entry` against the table size, resolves the table
BAR, maps the addressed 16-byte entry through the same
`CAP_MMIO_MAP`-gated `MmioMapper`, writes the message address/data and
clears the entry's per-vector mask, then sets the MSI-X Enable bit and
clears the function mask in the capability's Message Control register.
A table that lives in an I/O-port BAR is refused (`Unsupported`); an
entry index beyond the table or an entry that overruns its BAR fails
closed (`OutOfRange`); a caller without `CAP_MMIO_MAP` is denied
(`PermissionDenied`, propagated from the mapper). The driver never
synthesises a pointer.

The `MsiMessage` (address + data) is **opaque** to the bus driver: only
the architecture layer knows how to address its interrupt controller.
On x86, `rustos_arch_x86_64::irq::msi_message(vector, destination)`
encodes the local-APIC message format (physical destination, fixed
delivery, edge trigger; Intel SDM Vol 3A §11.11) — the `0xFEE`-prefixed
address selecting the destination CPU and the data carrying the chosen
external vector (`0x30..=0xFE`). A GIC or PLIC port would build a
different pair; the bus driver copies whichever it is given verbatim.

As with the virtio-window hand-off, ring 0 reaches `route_msix` through
a frozen ABI seam rather than the concrete type: `Pci<C>` implements
`rustos_abi::driver::msix::MsixBus` (a supertrait of `Bus`), so the
boot path can route a device's interrupt through a single `&dyn
MsixBus` without naming a concrete `drivers/bus/*` type
(`AGENTS.md` §8). `provision_and_run` calls it once per device — after
the four register windows are mapped and before the driver host is
built — so a routing failure fails the whole bring-up closed
(`VirtioPciWalkError::RouteMsix`) without loading a driver whose
`notify_wait` could never wake. Legacy MSI and INTx routing are not
implemented.

### Generic-PCI BAR hand-off (the xHCI / VL805 path)

A non-virtio PCI device — the Raspberry Pi 4's VL805 `PCIe` xHCI USB
host controller (`plans/PI.md` P10) — exposes its registers as a whole
BAR, not the virtio capability tuples, and drives DMA without MSI-X.
Its driver needs a smaller surface than `VirtioPciBus`: map one BAR,
and turn on bus mastering. `rustos_abi::driver::pci::PciBus` (a
supertrait of `Bus`) is that seam:

- `map_bar_window(bdf, bar_index, mapper)` resolves the memory BAR's
  probed base/length and maps it through the same `CAP_MMIO_MAP`-gated
  `MmioMapper` (refusing I/O-port and unused BARs). The resolved base is
  the address held in the BAR — a *PCIe-bus* address; turning it into a
  CPU mapping is the host bridge's job, so a bridge-aware `MmioMapper`
  (the Pi 4's `IdentityMmioMapper`, which applies the outbound `ranges`
  bus→CPU translation) does it, not this architecture-neutral walk;
- `enable_bus_master(bdf)` sets the function's Memory Space + Bus
  Master Enable bits (PCI Local Bus 3.0 §6.2.2) so the controller may
  issue the upstream DMA its rings live in.
- `assign_bar(bdf, bar_index, window_base, window_size)` assigns the
  BAR a base when firmware left it **unassigned** (address bits zero),
  placing it at the lowest size-aligned PCIe-bus address inside the host
  bridge's outbound window and returning that base. Firmware normally
  programs BARs, but resetting and re-enumerating a root complex (the
  BCM2711 PCIe bring-up) leaves a downstream function unassigned, so a
  map would target physical address 0; assigning resources from the
  bridge's window is the PCI core's job. It probes the
  BAR's size/type, writes both dwords for a 64-bit BAR (control bits
  preserved), and leaves an already-based BAR untouched (a no-op under
  QEMU or a firmware-assigned BAR). It fails closed (`OutOfRange`) if the
  size-aligned placement does not fit the window, and refuses I/O-port
  BARs (`Unsupported`).
- `read_config(bdf, offset)` reads a configuration-space dword back
  (the byte `offset` taken to its dword), a read-only diagnostic that
  touches no state. It confirms a prior write took effect — a
  just-assigned BAR, an enabled command register, a programmed bridge
  window — so a metal capture can tell a configuration write that did
  not stick apart from a device that does not decode despite correct
  programming (`AGENTS.md` §15.7). It reaches both the root-port bridge
  (bus 0) and a downstream function through the same windowed accessor.

`Pci<C>` implements `PciBus` by forwarding to the inherent
`map_bar_window` / `enable_bus_master` / `assign_bar` / `read_config`; `route_msix` calls the same
`enable_bus_master`, so the activation has one definition
(`AGENTS.md` §2.2). A device-class driver reaches the bus only through
`&dyn PciBus`, never naming the concrete `lib/pci` crate
(`AGENTS.md` §17.4).

The xHCI driver consumes it in `rustos_drv_bus_usb::wiring`. A
`devmgr`/host composition maps the discovered `brcm,bcm2711-pcie`
ECAM-access window, builds the bus over it (`mechanism_ecam`), and
hands the `&dyn PciBus` plus the discovered inbound-DMA aperture top and
the outbound PCIe window to
`open_discovered(host, bus, dma_aperture_top, outbound_window)`. That
function checks `CAP_MMIO_MAP`, enumerates for the USB-class function
(`0x0C03`), carves the device-shared DMA region from the host's DMA
facility and verifies it lies wholly **below** the aperture the bridge
lets devices reach (fail-closed `OutOfRange`, `AGENTS.md` §5.4), assigns
BAR0 inside the outbound window if firmware left it unassigned
(`assign_bar`), enables bus mastering, and maps BAR0 — these map-prefix
steps are factored into the public `map_controller`, returning the
mapped `MappedXhci { window, dma }` — and then brings the controller up
through `Xhci::open` + `UsbDevice::start`. The split lets the autoloaded
user-space keyboard driver read the controller's capability block and
report a mapping / open / start failure distinctly between the map and the
bring-up, without re-mapping the BAR (one window per device, `AGENTS.md` §2.2).
QEMU models no Pi USB timing (`AGENTS.md` §0.4), so the host tests prove
the composition and its fail-closed paths up to the controller hand-off;
the live controller bring-up is the on-metal acceptance item.

### Child-node emission into the hardware tree

A bus that enumerates downstream devices is responsible for growing the
hardware tree at runtime (`AGENTS.md` §18.1 / §18.3): each device it
finds becomes a child `HwNode` carrying the match keys a driver's signed
bind table resolves against, so a device behind the bus autoloads its
driver as match **data** rather than by a hand-wired composition module
(`AGENTS.md` §2.2 / §18.5). `PciBus::describe_function(bdf)` is that
seam: it reads the function's `vendor:device` and its **full 24-bit class
code** `(base_class << 16) | (sub_class << 8) | prog_if` — the prog-if
kept so an xHCI host (`0x0C_03_30`) is told apart from the older
OHCI/UHCI/EHCI USB host classes that share `0x0C_03`, exactly what the
generic xHCI driver's wildcard bind key needs — and returns an `HwNode`
carrying a single `HwMatchKey::pci`. The node's `HwDeviceClass` is derived
from the PCI base class (serial-bus and bridge → `Bus`); driver binding is
decided by the match key, not the class. An absent function (the all-ones
vendor sentinel) fails closed with `NotFound`, never a fabricated node
(`AGENTS.md` §2.9). The node's **identity is kernel-assigned on publish**:
`describe_function` returns it with placeholder id/parent, and the
`hw_emit_node` syscall stamps a fresh, collision-free id and the emitter's
own matched node as parent, so a bus driver can neither forge its tree
position nor collide with an existing id (`AGENTS.md` §4 / §5.4 / §18.1).
No resource capabilities are attached here either — those are minted at
the load gate. This is the PCI half of `plans/PI.md` Stage 4.HW item 5b.

The user-space BCM2711 PCIe bus driver (`drivers/bus/pcie_brcm`) drives
this seam end to end: it binds the discovered `brcm,bcm2711-pcie` node,
trains the link, locates the VL805 with the shared
`rustos_pci::find_function_by_class` scan, assigns/enables/maps its BAR
with `rustos_pci::assign_and_map_bar` (the one primitive the xHCI driver
also uses, `AGENTS.md` §2.2), resolves the BAR to its CPU-physical address
with `rustos_pci::bus_to_cpu_phys`, and publishes the controller as an
xHCI `HwNode` carrying that BAR (an `Mmio` window inside the bridge's
outbound `BusWindow` grant, so the kernel's grant-coverage check admits
it) and a DMA constraint. The composition lives — and is host-tested
against a mock bus — in the driver crate's own `lib` target
(`wiring::emit_vl805_node` / `wiring::publish_usb_function`), so the driver
binary is a thin freestanding stub.

The USB host driver does the same one level down, for the HID device it
enumerates behind the controller (`plans/PI.md` Stage 4.HW item 5b-ii).
A USB device's class lives on its *interface*, not its device
descriptor (whose `bDeviceClass` is `0` for an HID device), so
`UsbDevice::enumerate_hid` reads the configuration descriptor at its
exact advertised `wTotalLength` during bring-up (a 9-byte header read
first, then precisely that many bytes) and parses **every**
default-alternate interface descriptor (`InterfaceInfo::decode_all`,
walking the concatenated descriptors by each `bLength`, fail-closed on
a truncated, mistyped, or
interface-less reply). A composite device — a wireless keyboard+mouse
receiver carrying a boot-keyboard *and* a boot-mouse interface — gets one
device-table entry and one emitted node **per served interface**, the
siblings sharing the device's slot and EP0. The discovered
`bConfigurationValue` and each `bInterfaceNumber` drive
`SET_CONFIGURATION` and the per-interface HID `SET_PROTOCOL(boot)` —
neither is assumed to be `1` / `0` any more — and the 24-bit interface class
`(bInterfaceClass << 16) | (bInterfaceSubClass << 8) | bInterfaceProtocol`
(an HID boot keyboard is `0x03_01_01`, a boot mouse `0x03_01_02`) is
captured for emission. `UsbDevice::describe_device(parent_id, node_id)`
then returns an `HwNode` of class `Input`, parented at the controller's
node, carrying one `HwMatchKey::usb` of the device's `vid:pid` and that
captured interface class — never a fabricated one (`AGENTS.md` §18.5) —
so the `usb_kbd`/`usb_mouse` class-wildcard `BIND_KEYS` resolve
against it exactly as `devmgr` will. It fails closed with `NotFound`
before a device has been enumerated.

Together with the bus-driver `BIND_KEYS` (item 5a), the `devmgr` autoload
wiring (item 5c) is the data-driven path that **replaced** the former
in-kernel `usb_keyboard` composition scaffold (deleted at `plans/PI.md`
P10 D5d): the whole chain is now autoloaded user-space drivers.

## Constructing the real-hardware bus

The boot pipeline reaches PCI through a single public constructor,
`rustos_pci::mechanism_one(pio)`. It builds the bus over
configuration **mechanism #1** — the `0xCF8` address word / `0xCFC`
data word port pair (PCI Local Bus 3.0 §3.2.2.3.2) — and returns it as
`impl VirtioPciBus + MsixBus + PciBus`. All three traits have `Bus` as
a supertrait, so the value also coerces to `&dyn Bus`; the concrete
`Pci` type stays crate-private (`AGENTS.md` §8). The constructor is
architecture-neutral and carries no `cfg(target_arch …)` gate: the
`pio` argument is a `rustos_abi::PortIo` backend, and the only `in`/
`out` instructions live inside the architecture port that supplies it
(for x86_64, `rustos_arch_x86_64::pio::x86_port_io()`). This keeps the
driver free of inline assembly and target gates (`AGENTS.md` §17.2 /
§17.4). Construction performs no I/O — it only stores the supplied
backend — so it is sound to call before the host bridge has been
probed; configuration access happens lazily on the trait methods. Ring
0 hands the result to `rustos_kernel::provision_virtio_pci` /
`provision_and_run` as the `&dyn VirtioPciBus` + `&dyn MsixBus` device
bus. Non-x86 architectures reach PCIe through memory-mapped ECAM via
`mechanism_ecam(window)`, which performs no port I/O and exposes the
same `VirtioPciBus + MsixBus + PciBus` seams; the Pi 4's VL805 xHCI is
reached through its `PciBus` view (see above).

## Shared types

There is no copy-paste between the two drivers; the shared pieces of
code are the FDT parser (`lib/util::dtb`), the `RegisterWindow` /
`MmioMapper` register-window seam (`lib/abi`), and the `PortIo`
port-I/O seam (`lib/abi`), all of which live below the drivers because
more than one crate needs them (`AGENTS.md` §2.3). The `PortIo` seam
crossed into `lib/abi` once a second caller materialised — the x86_64
architecture port that implements it (`AGENTS.md` §17.2 / §17.4) — so
the PCI driver no longer carries the `in`/`out` instructions or a
target gate. PCI and MMIO still each keep their own
configuration-access abstraction (`ConfigSpace` / the MMIO slot
reader) inside their crate because no second caller for those has
materialised.
