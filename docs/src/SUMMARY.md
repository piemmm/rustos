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

# Security

- [Per-task capability registry](./security/captable.md)

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

# ABI

- [Driver traits (`abi-v1`)](./abi/driver_traits.md)

# Platforms

- [x86_64](./platform/x86_64.md)
