# Bus drivers

RustOS bus drivers enumerate the devices attached to a transport
(PCI, MMIO, virtio) and surface them to the userland driver host as
`BusDevice` records. They implement the single class trait
[`rustos_abi::driver::bus::Bus`] and nothing else — every other type
is `pub(crate)` per `AGENTS.md` §8.

## Stage-4 drivers in this class

| Crate                    | Platform              | Status   |
| ------------------------ | --------------------- | -------- |
| `drivers/bus/pci`        | x86_64 (PIO) / PCIe ECAM / BCM2711 windowed | Shipped  |
| `drivers/bus/pcie_brcm`  | Pi 4 (BCM2711 RC)     | P10 link bring-up (host-proven); metal pending |
| `drivers/bus/mmio`       | aarch64 / riscv64     | Shipped  |
| `drivers/bus/virtio`     | cross-arch            | Stage 4.D |
| `drivers/bus/usb`        | Pi 4 (VL805 xHCI)     | P10 protocol layers + HID enumeration (host-proven) |

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

## PCI driver — `drivers/bus/pci`

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
over forwarded config would wedge the boot CPU. The gate mirrors Linux
`brcm_pcie_map_conf` returning `NULL` for a non-zero slot on a non-root
bus.

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
be brought up. `drivers/bus/pcie_brcm` performs that bring-up over the
BCM2711 root-complex registers.

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

Hold the bridge in reset and assert `PERST#`; release the bridge
reset; clear the SerDes `IDDQ` power-down and let it settle; program
`MISC_CTRL` (SCB access, UR config reads, 128-byte burst, RCB modes);
program the inbound (PCIe→system-memory) viewport `RC_BAR2` from the
discovered `dma-ranges` (the size encoded by `encode_ibar_size`, the
size rounded up to a power of two); disable the unused `RC_BAR1` /
`RC_BAR3` inbound windows; confirm the root-port role (fail closed
with `DeviceFault` otherwise); advertise ASPM L0s+L1 and present the
root complex as a PCI-PCI bridge; program the bridge bus-number register
(primary 0, secondary/subordinate = the single downstream bus) so the
port forwards configuration to the directly-attached VL805; program the
outbound (CPU→PCIe) MMIO window from the discovered `ranges`; deassert
`PERST#`; then poll
`MISC_PCIE_STATUS` for data-link-active + phy-link-up, bounded by
`DEFAULT_LINK_POLLS` (100 ms) and failing closed if the link never
trains. All windows are device-tree-discovered, never compiled-in
(`AGENTS.md` §18.1).

### Composition

`wiring::open_discovered` maps the discovered controller window under
`CAP_MMIO_MAP` and runs the bring-up; the caller then recovers the
window (`into_regs`) and builds `mechanism_brcm(window)` to enumerate
the VL805. The crate performs only the link bring-up and so never
depends on another driver crate (`AGENTS.md` §17.4). Both windows the
`PcieWindows` carries are device-tree-discovered: the inbound aperture
from the node's `dma-ranges` (an `HwResource::dma_translated` carrying
the CPU-reachability top, extent, and the inbound PCIe-space base) and
the outbound MMIO window from its `ranges` (an `HwResource::bus_window`
carrying the CPU base, size, and far-side PCIe base —
`kernel/arch/aarch64::fdt::{dma_ranges_aperture,outbound_mmio_window}`,
`AGENTS.md` §18.1).

The whole chain is composed in `kernel/rustos-kernel::usb_keyboard`
(the image-assembly seam is the one crate permitted to name the four
driver crates across strata, `AGENTS.md` §17.4 / §8):
`pcie_bringup_from_node` reads the three resources off the discovered
`brcm,bcm2711-pcie` `HwNode` into a `PcieBringup`, a `ChainHost` lends
the bus driver the kernel's capability-gated MMIO mapper + per-driver
DMA host, and `bring_up_keyboard` runs link-train → `mechanism_brcm` →
`usb::wiring::open_discovered` → `enumerate_first_connected`, yielding a
`BootKeyboard` whose decoded bytes a `QueueConsoleSink` feeds into the
video console's input queue (`console_input`/`VIDEO_KEYBOARD`). That
engine is host-tested up to the controller hand-off, where the inert
mock register window faults — the metal boundary. The remaining
follow-up is the aarch64 boot-path invocation (assembling the concrete
`DriverHost` + a generic-timer `Delay` and looping `pump_once`); QEMU
models no Pi PCIe link timing or USB, so metal acceptance is a checklist
(`plans/PI.md` P10).

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

## xHCI driver — `drivers/bus/usb`

The Pi 4 reaches its USB-A ports through a VL805 PCIe xHCI controller
(`plans/PI.md` P10). The crate carries the host-provable xHCI protocol
layers and the single-device HID enumeration engine; the PCI BAR /
hwtree wiring for the VL805 is the remaining P10 increment, and QEMU
models no Pi USB timing, so the host suite is the emulation artefact
and metal acceptance stays a checklist.

### Register seam and bring-up

Every controller access goes through the crate's `XhciHost` seam —
implemented for the kernel-minted `RegisterWindow` on metal, and for a
register-level mock in tests (the `emmc2` `SdhciHost` shape, `AGENTS.md`
§2.2). `Xhci::open` runs the xHCI §4.2 prologue: validate the
capability block (`CAPLENGTH`/`HCIVERSION` plausibility, non-zero
`MaxSlots`/`MaxPorts`/`DBOFF`/`RTSOFF` — the absent-controller
all-ones read fails here), wait for Controller-Not-Ready to clear,
halt a running controller, then issue the self-clearing Host
Controller Reset. Every wait is poll-budget-bounded and fails closed
with `DeviceFault` (`AGENTS.md` §2.1); the controller is left halted.
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

`device` is the single-device enumeration engine. All device-shared
bytes live in one caller-provided region behind the crate's
`DmaRegion` seam — implemented for the `lib/abi` `DmaSlab` in
production and by a plain shared buffer in tests — and the engine
computes a 64-byte-aligned `Layout` inside it (DCBAA, ERST, command
ring, event segment, input/output contexts, EP0 and interrupt-IN
transfer rings, the control data buffer, and per-slot report
buffers), refusing a region that is misaligned or too small.

`UsbDevice::start` zeroes the region, publishes the ERST entry and
the rings' Link TRBs, and starts the controller through `Xhci::start`.
`UsbDevice::enumerate_hid(port)` then brings the device on a root-hub
port to the configured boot-protocol state (§4.3): port reset when the
port is not yet enabled, Enable Slot (validating the returned slot
ID), Address Device (input control context `A0 | A1`, slot context,
EP0 context with the speed-derived max packet size),
`GET_DESCRIPTOR(device)` (decoded fail-closed — a forged length, type,
or zero-configuration descriptor is `BadMagic`), Configure Endpoint
for the interrupt-IN endpoint (DCI 3), `SET_CONFIGURATION(1)`,
`SET_PROTOCOL(boot)`, and finally a primed interrupt-IN ring. Control
transfers carry the SETUP payload as immediate data, set
Interrupt-on-Short-Packet on the IN data stage, and watch only the
addresses of their own in-flight TRBs: a completion for a TRB never
issued, an undecodable completion code, an unexpected event type, or a
stalled request is a `DeviceFault`, and every wait is bounded by the
engine's poll budget (`AGENTS.md` §2.1 / §2.9).

`UsbDevice` implements the `rustos_abi::driver::input::ReportSource`
seam (hoisted into `lib/abi` because its consumer,
`drivers/input/usb_hid`, is a sibling driver and drivers depend only
on `lib/*`, `AGENTS.md` §17.4): `next_report` consumes one transfer
event, validates the controller's claim end to end (slot, endpoint
ID, completion code, TRB address inside the interrupt ring, residual
within the TRB length — §5.4), copies the report out of the slot's
buffer, retires and re-arms the ring, and rings the endpoint doorbell,
so the boot-protocol decoders poll reports straight off the transfer
ring. The crate's tests prove the whole chain against the
register-level mock plus an in-memory ring model sharing the same
buffer — including a `BootKeyboard` polling decoded key events over
the mock controller — plus the fail-closed paths (forged residual,
stalled class request, empty port, double enumeration, undersized or
misaligned DMA region).

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

`Pci<C>` implements `PciBus` by forwarding to the inherent
`map_bar_window` / `enable_bus_master`; `route_msix` calls the same
`enable_bus_master`, so the activation has one definition
(`AGENTS.md` §2.2). A device-class driver reaches the bus only through
`&dyn PciBus`, never naming the concrete `drivers/bus/pci` crate
(`AGENTS.md` §8 / §17.4).

The xHCI driver consumes it in `rustos_drv_bus_usb::wiring`. A
`devmgr`/host composition maps the discovered `brcm,bcm2711-pcie`
ECAM-access window, builds the bus over it (`mechanism_ecam`), and
hands the `&dyn PciBus` plus the discovered inbound-DMA aperture top to
`open_discovered(host, bus, dma_aperture_top)`. That function checks
`CAP_MMIO_MAP`, enumerates for the USB-class function (`0x0C03`), carves
the device-shared DMA region from the host's DMA facility and verifies
it lies wholly **below** the aperture the bridge lets devices reach
(fail-closed `OutOfRange`, `AGENTS.md` §5.4), enables bus mastering,
maps BAR0, and brings the controller up through `Xhci::open` +
`UsbDevice::start`. QEMU models no Pi USB timing (`AGENTS.md` §0.4), so
the host tests prove the composition and its fail-closed paths up to
the controller hand-off; the live controller bring-up is the on-metal
acceptance item.

### Child-node emission into the hardware tree

A bus that enumerates downstream devices is responsible for growing the
hardware tree at runtime (`AGENTS.md` §18.1 / §18.3): each device it
finds becomes a child `HwNode` carrying the match keys a driver's signed
bind table resolves against, so a device behind the bus autoloads its
driver as match **data** rather than by a hand-wired composition module
(`AGENTS.md` §2.2 / §18.5). `PciBus::describe_function(bdf, parent_id,
node_id)` is that seam: it reads the function's `vendor:device` and its
**full 24-bit class code** `(base_class << 16) | (sub_class << 8) |
prog_if` — the prog-if kept so an xHCI host (`0x0C_03_30`) is told apart
from the older OHCI/UHCI/EHCI USB host classes that share `0x0C_03`,
exactly what the generic xHCI driver's wildcard bind key needs — and
returns an `HwNode` parented at `parent_id` with a single
`HwMatchKey::pci`. The node's `HwDeviceClass` is derived from the PCI
base class (serial-bus and bridge → `Bus`); driver binding is decided by
the match key, not the class. An absent function (the all-ones vendor
sentinel) fails closed with `NotFound`, never a fabricated node
(`AGENTS.md` §2.9). The tree owner allocates the ids and attaches no
resource capabilities here — those are minted at the load gate
(`AGENTS.md` §4 / §5.4). This is the PCI half of `plans/PI.md`
Stage 4.HW item 5b.

The USB host driver does the same one level down, for the HID device it
enumerates behind the controller (`plans/PI.md` Stage 4.HW item 5b-ii).
A USB device's class lives on its *interface*, not its device
descriptor (whose `bDeviceClass` is `0` for an HID device), so
`UsbDevice::enumerate_hid` reads the configuration descriptor during
bring-up and parses its first interface descriptor
(`InterfaceInfo::decode`, walking the concatenated descriptors by each
`bLength`, fail-closed on a truncated, mistyped, or interface-less
reply). The discovered `bConfigurationValue` and `bInterfaceNumber`
drive `SET_CONFIGURATION` and the HID `SET_PROTOCOL(boot)` — neither is
assumed to be `1` / `0` any more — and the 24-bit interface class
`(bInterfaceClass << 16) | (bInterfaceSubClass << 8) | bInterfaceProtocol`
(an HID boot keyboard is `0x03_01_01`, a boot mouse `0x03_01_02`) is
captured for emission. `UsbDevice::describe_device(parent_id, node_id)`
then returns an `HwNode` of class `Input`, parented at the controller's
node, carrying one `HwMatchKey::usb` of the device's `vid:pid` and that
captured interface class — never a fabricated one (`AGENTS.md` §18.5) —
so `usb_hid::BIND_KEYS`'s class-wildcard keyboard/mouse keys resolve
against it exactly as `devmgr` will. It fails closed with `NotFound`
before a device has been enumerated.

Together with the bus-driver `BIND_KEYS` (item 5a), `devmgr` autoload
wiring (item 5c) closes the data-driven path that supersedes the
`kernel/rustos-kernel::usb_keyboard` composition scaffold.

## Constructing the real-hardware bus

The boot pipeline reaches PCI through a single public constructor,
`rustos_drv_bus_pci::mechanism_one(pio)`. It builds the bus over
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
