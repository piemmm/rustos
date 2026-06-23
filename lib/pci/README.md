# `rustos-pci`

**Stability tier: stable.** The public surface is the three
`mechanism_*` constructors, the frozen `abi-v1` bus/transport seams
(`Bus`, `VirtioPciBus`, `MsixBus`, `PciBus`) they return, and the shared
bus-driver locate primitives (`find_function_by_class`,
`assign_and_map_bar`, `bus_to_cpu_phys`, and the `USB_CONTROLLER_CLASS`
PCI class code) the xHCI driver and the BCM2711 PCIe bus driver both use;
changing the seam surface is governed by `AGENTS.md` §9.

PCI/PCIe configuration-access **library** (`lib/*`, not a driver crate):
it enumerates devices and walks each function's capability list to
surface MSI / MSI-X descriptors, virtio-1.x configuration structures,
and BAR (Base Address Register) windows, and it assigns/maps BARs and
enables bus-mastering for a DMA-driving device driver. It lives in
`lib/` because PCI configuration access is shared bus-protocol logic a
`drivers/*` crate may not reach through a sibling driver (`AGENTS.md`
§17.4) — the kernel boot pipeline, the user-space `drivers/bus/pcie_brcm`
driver, and the host tests all compose it through the seams above. This
mirrors `lib/usb` ↔ `drivers/bus/usb`.

Configuration space is reached through one of three access mechanisms,
selected at construction by the caller:

- **Mechanism #1** (`0xCF8` / `0xCFC`, x86_64) — the legacy I/O-port
  bridge (`mechanism_one`), behind the `rustos_abi::PortIo` seam.
- **ECAM / MMCONFIG** (`mechanism_ecam`) — PCIe enhanced configuration
  access: configuration space is mapped flat into MMIO, one 4 KiB
  block per `(bus, device, function)`, reached through a
  capability-checked `rustos_abi::RegisterWindow`.
- **BCM2711 windowed** (`mechanism_brcm`) — the Raspberry Pi 4 root
  complex's index/data window pair inside the controller's own register
  block, used to reach its VL805 USB host controller after the link is
  trained.

The enumeration, capability-walk, BAR-sizing, and window/MSI-X hand-off
core is mechanism-agnostic: it is parameterised over the `ConfigSpace`
trait, which all three bridges implement.

## Supported hardware

| Class             | Tested against                                    |
| ----------------- | -------------------------------------------------- |
| PCI host bridge   | QEMU `q35` (`Intel 82G33`, `vendor:device = 8086:29C0`) |
| LPC bridge        | QEMU `q35` (`8086:2918`)                           |
| AHCI SATA         | QEMU `q35` (`8086:2922`)                           |
| SMBus             | QEMU `q35` (`8086:2930`)                           |
| virtio-net-pci    | `1AF4:1041` (modern transitional)                  |
| virtio-blk-pci    | `1AF4:1042` (modern, virtio-1.x cap layout)        |
| VL805 xHCI (PCIe) | Pi 4 (BCM2711) USB host, `1106:3483`, over ECAM    |

The `q35` fixture in `src/tests.rs` reproduces that PCI topology byte
for byte over mechanism #1; the VL805 fixture lays a flat ECAM region
(a root-port bridge plus the `1106:3483` xHCI) and drives the same
enumeration core over `EcamConfigSpace`. The live surface uses the
same core through the [`Bus`] trait.

## Required capabilities

The library requests no capability of its own and holds no ambient
authority (`AGENTS.md` §4). It reads or writes I/O ports / MMIO only
through the `rustos_abi::PortIo` / `rustos_abi::MmioMapper` seams its
caller supplies, and a BAR or MSI-X window is mapped only by routing the
request through the kernel MMIO-map facility, which enforces
`CAP_MMIO_MAP` (`AGENTS.md` §4 — the library never synthesises a
pointer).

## Limitations

- A memory BAR is mapped only by routing the request through the
  kernel MMIO-map facility (`map_bar_window`); the driver never
  synthesises a pointer (`AGENTS.md` §4).
- virtio-1.x vendor-specific capabilities (`cap_id = 0x09`) are
  decoded into `(bar, offset, length)` triples; `map_virtio_window`
  resolves a requested `cfg_type` (common / notify / ISR / device)
  to a capability-checked register window for the virtio PCI
  transport, and `virtio_notify_off_multiplier` surfaces the
  notification scale.
- MSI / MSI-X capabilities are **discovered** by the capability walk;
  MSI-X is additionally **routable**: `route_msix` programs a table
  entry with a kernel-supplied `MsiMessage`, unmasks the entry, and
  sets the function's MSI-X Enable bit (clearing the function mask).
  The table write goes through the kernel MMIO-map facility — the
  driver never synthesises a pointer (`AGENTS.md` §4). The message
  itself is built by the architecture layer (e.g.
  `rustos_arch_x86_64::irq::msi_message`); the bus driver copies it
  verbatim. Legacy MSI and INTx routing are not implemented.
  Ring 0 reaches `route_msix` through the frozen `abi-v1`
  `rustos_abi::MsixBus` seam.
- The library holds no global state: all state lives in the `Pci<C>`
  instance a `mechanism_*` constructor returns and the composing host
  owns, so it is freely reused across the kernel, a user-space bus
  driver, and tests.

## Tests

`cargo test -p rustos-pci` runs:

- The PIO-bridge round-trip test against a recording mock, and the
  ECAM offset-encoding + round-trip / out-of-window sentinel tests.
- The exact `q35` device-list assertion (mechanism #1) and the
  VL805-over-ECAM enumeration + MSI-X capability-decode assertions.
- The BCM2711 windowed-access round-trip and bus-bound refusal tests.
- Capability-list and BAR-sizing walkers, including the virtio-1.x
  configuration-structure decode.
- The MSI-X routing hand-off: programming a table entry + enabling
  the function, plus the not-found / out-of-range / I/O-BAR /
  capability-denied failure paths.
- The memory-BAR and virtio-config register-window hand-offs to a
  mock MMIO mapper (including the capability-denial path).
- BAR assignment inside a bridge outbound window + `describe_function`
  child-node synthesis.
- The shared locate primitives: `find_function_by_class` (first match /
  fail-closed not-found), `assign_and_map_bar` (assign → enable → map
  order), and `bus_to_cpu_phys` (outbound-window translation, in-window
  and fail-closed-out-of-window).

## License

GPL-2.0-or-later, with the `RustOS-syscall-note` syscall / ABI exception
(see the repository-root `LICENSE`).
