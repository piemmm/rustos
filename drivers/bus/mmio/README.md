# `rustos-drv-bus-mmio`

Memory-mapped IO bus driver. Enumerates virtio-MMIO transport slots
described by the boot-time flat device-tree blob (FDT v17) the
kernel hands to the driver host through the boot capability.

## Supported hardware

| Class                  | Tested against                                |
| ---------------------- | --------------------------------------------- |
| virtio-mmio transport  | QEMU `aarch64 -M virt`, `riscv64 -M virt`     |

Each `virt` machine exposes 32 transport slots starting at
`0x0A00_0000`, each 0x200 bytes wide; the driver visits every
`compatible = "virtio,mmio"` node, probes the
`MagicValue` / `Version` / `DeviceID` / `VendorID` window through a
volatile reader, and reports back the populated slots.

## Required capabilities

| Capability       | When                                              |
| ---------------- | -------------------------------------------------- |
| `CAP_DRV_LOAD`   | At `register` time. The host gates this.          |

The driver performs no MMIO reads until the host first calls into
the `Bus` trait that `register` clears. No ambient authority is
requested (`AGENTS.md` §4).

## Limitations

- Targets the `virt`-style `#address-cells = 2 / #size-cells = 2`
  device-tree layout. The walker fails closed
  (`DriverError::DeviceFault`) on a malformed `reg` property — a
  hostile blob cannot silently under-enumerate.
- The volatile reader is bounds-checked against the mapped window
  passed at construction; reads outside it return the "empty slot"
  sentinel rather than dereferencing out of range.
- Loadable, unloadable, and reloadable at runtime
  (`AGENTS.md` §8) — the driver holds no global state beyond the
  `Mmio<T>` instance the host owns.

## Tests

`cargo test -p rustos-drv-bus-mmio` runs:

- Volatile-reader round-trip against an aligned host buffer.
- The exact `virt` device-list assertion (four-slot fixture, two
  populated, two empty).
- DTB-driven walker behaviour for malformed slots and empty trees.
- The `register` capability gate.

## License

GPL-2.0-or-later, with the `RustOS-syscall-note` syscall / ABI exception
(see the repository-root `LICENSE`).
