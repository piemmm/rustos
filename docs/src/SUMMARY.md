# Summary

[Introduction](./introduction.md)

# Project

- [Contributing](./contributing.md)

# Architecture

- [System overview](./architecture/overview.md)
- [Kernel entry, init order, and panic policy](./architecture/kernel.md)
- [Kernel synchronisation primitives](./architecture/sync.md)
- [Kernel memory subsystem](./architecture/memory.md)
- [Kernel scheduler](./architecture/scheduler.md)
- [Kernel security subsystem](./architecture/security.md)
- [Kernel IPC subsystem](./architecture/ipc.md)
- [Kernel syscall subsystem](./architecture/syscalls.md)
- [Modularity contracts and enforcement](./architecture/modularity.md)

# Security

- [Per-task capability registry](./security/captable.md)
- [Hardware interrupts: capability-gated wake-ups](./security/irq.md)
- [Audit-log integrity](./security/audit_log.md)
- [Supply-chain integrity: the SBOM](./security/supply_chain.md)

# Shared libraries

- [Overview](./lib/overview.md)
  - [`rustos-abi`](./lib/abi.md)
  - [`rustos-caps`](./lib/caps.md)
  - [`rustos-collections`](./lib/collections.md)
  - [`rustos-crypto`](./lib/crypto.md)
  - [`rustos-log`](./lib/log.md)
  - [`rustos-util`](./lib/util.md)

# Drivers

- [Overview](./drivers/overview.md)
- [Userland driver host](./drivers/host.md)
- [Driver lifecycle](./drivers/lifecycle.md)
- [Bus drivers](./drivers/bus.md)
- [Virtio transport](./drivers/virtio.md)
- [Block drivers](./drivers/block.md)
- [Network drivers](./drivers/network.md)
- [Display drivers](./drivers/display.md)
- [Input drivers](./drivers/input.md)

# Userland

- [Networking service](./userland/net_icmp.md)

# ABI

- [Driver traits (`abi-v1`)](./abi/driver_traits.md)

# Platforms

- [x86_64](./platform/x86_64.md)
- [riscv64](./platform/riscv64.md)
- [aarch64](./platform/aarch64.md)
- [wasm32](./platform/wasm32.md)
