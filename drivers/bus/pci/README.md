# `rustos-drv-bus-pci`

PCI/PCIe bus driver. Enumerates devices on x86_64 platforms through
the configuration-access **mechanism #1** (`0xCF8` / `0xCFC`) and
walks each function's capability list to surface MSI / MSI-X
descriptors, virtio-1.x configuration structures, and BAR (Base
Address Register) windows.

## Supported hardware

| Class             | Tested against                                    |
| ----------------- | -------------------------------------------------- |
| PCI host bridge   | QEMU `q35` (`Intel 82G33`, `vendor:device = 8086:29C0`) |
| LPC bridge        | QEMU `q35` (`8086:2918`)                           |
| AHCI SATA         | QEMU `q35` (`8086:2922`)                           |
| SMBus             | QEMU `q35` (`8086:2930`)                           |
| virtio-net-pci    | `1AF4:1041` (modern transitional)                  |
| virtio-blk-pci    | `1AF4:1042` (modern, virtio-1.x cap layout)        |

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
- A memory BAR is mapped only by routing the request through the
  kernel MMIO-map facility (`map_bar_window`); the driver never
  synthesises a pointer (`AGENTS.md` §4).
- virtio-1.x vendor-specific capabilities (`cap_id = 0x09`) are
  decoded into `(bar, offset, length)` triples; `map_virtio_window`
  resolves a requested `cfg_type` (common / notify / ISR / device)
  to a capability-checked register window for the virtio PCI
  transport, and `virtio_notify_off_multiplier` surfaces the
  notification scale.
- MSI / MSI-X capabilities are **discovered** but never enabled.
- Loadable, unloadable, and reloadable at runtime (`AGENTS.md` §8) —
  the driver holds no global state beyond the `Pci<C>` instance the
  host owns.

## Tests

`cargo test -p rustos-drv-bus-pci` runs:

- The PIO-bridge round-trip test against a recording mock.
- The exact `q35` device-list assertion.
- Capability-list and BAR-sizing walkers, including the virtio-1.x
  configuration-structure decode.
- The memory-BAR and virtio-config register-window hand-offs to a
  mock MMIO mapper (including the capability-denial path).
- The `register` capability gate.

## License

GPL-3.0-only.
