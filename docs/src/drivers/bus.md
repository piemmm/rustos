# Bus drivers

RustOS bus drivers enumerate the devices attached to a transport
(PCI, MMIO, virtio) and surface them to the userland driver host as
`BusDevice` records. They implement the single class trait
[`rustos_abi::driver::bus::Bus`] and nothing else — every other type
is `pub(crate)` per `AGENTS.md` §8.

## Stage-4 drivers in this class

| Crate                    | Platform              | Status   |
| ------------------------ | --------------------- | -------- |
| `drivers/bus/pci`        | x86_64                | Shipped  |
| `drivers/bus/mmio`       | aarch64 / riscv64     | Shipped  |
| `drivers/bus/virtio`     | cross-arch            | Stage 4.D |

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

Mechanism #1 (PCI Local Bus 3.0 §3.2.2.3.2):

| Port  | Purpose                       |
| ----- | ----------------------------- |
| `0xCF8` | 32-bit configuration address |
| `0xCFC` | 32-bit configuration data    |

The crate splits the `in`/`out` instructions behind an internal
`PortIo` trait so the in-crate unit tests can exercise the bridge
against a recording mock without touching real I/O ports. The only
`unsafe` blocks in the crate live in `X86PortIo::read32` /
`X86PortIo::write32`; both carry `// SAFETY:` blocks documenting the
PCI-port and `nomem`/`nostack`/`preserves_flags` invariants and are
covered by the round-trip mock test.

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

## MMIO driver — `drivers/bus/mmio`

### DTB iterator

The boot DTB is parsed once through `rustos_util::dtb::Dtb`. The
parser is the shared `lib/util` module promoted in Stage 4 once two
callers materialised (this driver and the future platform-discovery
code); it validates the FDT v17 header, refuses span-out-of-range
blobs, and never panics. The MMIO driver walks every node, filters
on `compatible = "virtio,mmio"`, reads `reg = <base length>`, then
probes the four-register identifier window through the volatile
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
mapper is `KernelMmioMapper` in `drivers/bus/virtio` (behind the
`kernel-host` feature); it wraps `kernel/mem::MmioMap` and routes
every request through the capability gate `kernel/sec::map_mmio`.

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
mapper)` (in `kernel/rustos-kernel/src/virtio_pci_walk.rs`) takes a
`&dyn VirtioPciBus`, enumerates the bus into a bounded table, picks the
first function matching `VIRTIO_PCI_VENDOR_ID` and the requested device
ID, maps the four windows through the `CAP_MMIO_MAP`-gated `MmioMapper`,
reads the notify multiplier, and builds a `PciTransport`. Ring 0 thus
never names a concrete `drivers/bus/*` type and holds no ambient
authority — the capability check lives in the mapper, and every failure
is a typed `VirtioPciWalkError` rather than a panic (`AGENTS.md` §2.9).
The constants live once in `rustos_abi`; the driver's `VIRTIO_CFG_*`
names bind to them rather than re-stating the literals (`AGENTS.md`
§2.2).

### Boot wiring

`provision_virtio_pci` yields a transport, but a virtio-class driver
also needs a per-process DMA host and a driver host to run its signed
`.rxe`. `kernel/rustos-kernel/src/virtio_boot.rs` joins the three:
`provision_and_run(config, make_table, body)` takes a
`VirtioBootConfig` bundling the bus, the per-driver `MmioMap`, the DMA
frame allocator + direct physical map, the device's bound `IrqHandle`,
and the driver-host trust inputs. It builds a `KernelMmioMapper`,
provisions the `PciTransport`, constructs a `KernelVirtioFactory`, and
hands a live `drvhost::Host` (with the factory wired into
`HostConfig::virtio_host_factory`) plus the transport to the `body`
closure. The scope/callback shape keeps the mapper, factory, and host —
and every per-driver DMA pool the factory mints — on one boot frame, so
all of it is reclaimed when `body` returns and no driver retains a
register window or DMA mapping past its load (`AGENTS.md` §4). The boot
walk fails closed with a `VirtioPciWalkError` and never constructs the
host if the device or a window cannot be resolved.

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
(`AGENTS.md` §8). Legacy MSI and INTx routing are not implemented.

## Shared types

There is no copy-paste between the two drivers; the only shared
pieces of code are the FDT parser (`lib/util::dtb`) and the
`RegisterWindow` / `MmioMapper` register-window seam (`lib/abi`),
both of which live below the drivers because more than one crate
needs them (`AGENTS.md` §2.3). PCI and MMIO each keep their own
configuration-access abstraction inside the crate because no second
caller has materialised.
