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
- [Fuzzing the untrusted-input surface](./security/fuzzing.md)
- [Stateful models for the capability core](./security/proptest_models.md)
- [The capability + IPC model (Silver)](./security/model/capability_ipc.md)
- [The rxe loader: W^X, PIE, KASLR, CFI](./security/rxe_loader.md)
- [Side-channel mitigations (Arch HAL)](./security/side_channels.md)
- [Memory tagging (Arch HAL)](./security/memory_tagging.md)

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

# Filesystem

- [Overview](./filesystem/overview.md)
- [On-disk layout enforcement](./filesystem/layout.md)
- [Permissions](./filesystem/permissions.md)
- [FAT32 driver](./filesystem/fat32.md)
- [rustfs driver](./filesystem/rustfs.md)
- [ext4 driver](./filesystem/ext4.md)
- [POSIX conformance suite](./filesystem/posix_suite.md)

# Userland

- [PID 1 service manager](./userland/init.md)
- [System Information service](./userland/sysinfod.md)
- [Networking service](./userland/net_icmp.md)
- [Default shell](./userland/shell.md)

# ABI

- [Driver traits (`abi-v1`)](./abi/driver_traits.md)
- [System Information API (`sysinfo-v1`)](./abi/sysinfo.md)

# Platforms

- [x86_64](./platform/x86_64.md)
- [riscv64](./platform/riscv64.md)
- [aarch64](./platform/aarch64.md)
- [wasm32](./platform/wasm32.md)
