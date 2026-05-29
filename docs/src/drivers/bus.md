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

## Shared types

There is no copy-paste between the two drivers; the only shared
piece of code is the FDT parser, which lives in `lib/util::dtb`
because two independent crates need it (`AGENTS.md` §2.3). PCI and
MMIO each carry their own configuration-access abstraction inside
the crate because no second caller has materialised.
