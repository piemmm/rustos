# `rustos-drv-bus-pci`

PCI/PCIe bus driver. Enumerates devices on x86_64 platforms through
the configuration-access **mechanism #1** (`0xCF8` / `0xCFC`) and
walks each function's capability list to surface MSI / MSI-X
descriptors and BAR (Base Address Register) windows.

## Supported hardware

| Class             | Tested against                                    |
| ----------------- | -------------------------------------------------- |
| PCI host bridge   | QEMU `q35` (`Intel 82G33`, `vendor:device = 8086:29C0`) |
| LPC bridge        | QEMU `q35` (`8086:2918`)                           |
| AHCI SATA         | QEMU `q35` (`8086:2922`)                           |
| SMBus             | QEMU `q35` (`8086:2930`)                           |
| virtio-net-pci    | `1AF4:1041` (modern transitional)                  |

The fixture in `src/tests.rs` reproduces the exact `q35` PCI topology
byte for byte; the live-QEMU surface uses the same enumeration core
through the [`Bus`] trait.

## Required capabilities

| Capability       | When                                              |
| ---------------- | -------------------------------------------------- |
| `CAP_DRV_LOAD`   | At `register` time. The host gates this.          |

The driver does not read or write any I/O port until the host first
calls into the `Bus` trait that `register` clears. No ambient
authority is requested (`AGENTS.md` §4).

## Limitations

- x86_64-only. ECAM (PCIe enhanced configuration access) is **not**
  implemented; that is Stage 4.D scope.
- BARs are **discovered** but never mapped; mapping requests are
  routed through the driver host's memory capability by the upper
  driver (virtio-blk / virtio-net) in Stage 4.D.
- MSI / MSI-X capabilities are **discovered** but never enabled.
- Loadable, unloadable, and reloadable at runtime (`AGENTS.md` §8) —
  the driver holds no global state beyond the `Pci<C>` instance the
  host owns.

## Tests

`cargo test -p rustos-drv-bus-pci` runs:

- The PIO-bridge round-trip test against a recording mock.
- The exact `q35` device-list assertion.
- Capability-list and BAR-sizing walkers.
- The `register` capability gate.

## License

MIT OR Apache-2.0.
