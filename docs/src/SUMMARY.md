# Summary

[Introduction](./introduction.md)

# Project

- [Contributing](./contributing.md)

# Architecture

- [System overview](./architecture/overview.md)
- [Kernel entry, init order, and panic policy](./architecture/kernel.md)
- [Kernel panic diagnostics (registers + backtrace)](./architecture/panic-diagnostics.md)
- [User-fault kill diagnostics (identity, cause, offset)](./architecture/fault-diagnostics.md)
- [Kernel synchronisation primitives](./architecture/sync.md)
- [Kernel memory subsystem](./architecture/memory.md)
- [Kernel scheduler](./architecture/scheduler.md)
- [Kernel multitasking and the kthread runtime](./architecture/multitasking.md)
- [Kernel security subsystem](./architecture/security.md)
- [Kernel IPC subsystem](./architecture/ipc.md)
- [Kernel syscall subsystem](./architecture/syscalls.md)
- [Resource limits and scalability](./architecture/resource-limits.md)
- [CPU feature detection and self-optimising dispatch](./architecture/cpu-feature-dispatch.md)
- [Modularity contracts and enforcement](./architecture/modularity.md)
- [The pre-boot Supervisor console](./architecture/supervisor.md)

# Security

- [The capability lifecycle](./security/capabilities.md)
- [Per-task capability registry](./security/captable.md)
- [Hardware interrupts: capability-gated wake-ups](./security/irq.md)
- [Audit-log integrity](./security/audit_log.md)
- [Supply-chain integrity: the SBOM](./security/supply_chain.md)
- [Fuzzing the untrusted-input surface](./security/fuzzing.md)
- [The parser sandbox: minimum-capability workers](./security/sandbox.md)
- [Stateful models for the capability core](./security/proptest_models.md)
- [The capability + IPC model (Silver)](./security/model/capability_ipc.md)
- [The rxe loader: W^X, PIE, KASLR, CFI](./security/rxe_loader.md)
- [Side-channel mitigations (Arch HAL)](./security/side_channels.md)
- [Memory tagging (Arch HAL)](./security/memory_tagging.md)
- [Network security posture](./security/network.md)

# Shared libraries

- [Overview](./lib/overview.md)
  - [`tairix-abi`](./lib/abi.md)
  - [`tairix-appload`](./lib/appload.md)
  - [`tairix-binfmt`](./lib/binfmt.md)
  - [`tairix-bootload`](./lib/bootload.md)
  - [`tairix-caps`](./lib/caps.md)
  - [`tairix-collections`](./lib/collections.md)
  - [`tairix-complete`](./lib/complete.md)
  - [`tairix-cpuops`](./lib/cpuops.md)
  - [`tairix-crypto`](./lib/crypto.md)
  - [`tairix-curses`](./lib/curses.md)
  - [`tairix-devmatch`](./lib/devmatch.md)
  - [`tairix-disasm`](./lib/disasm.md)
  - [`tairix-dma-barrier`](./lib/dma_barrier.md)
  - [`tairix-drvrt`](./lib/drvrt.md)
  - [`tairix-fbcon`](./lib/fbcon.md)
  - [`tairix-help`](./lib/help.md)
  - [`tairix-hid`](./lib/hid.md)
  - [`tairix-keymap`](./lib/keymap.md)
  - [`tairix-log`](./lib/log.md)
  - [`tairix-multiboot2`](./lib/multiboot2.md)
  - [`tairix-net`](./lib/net.md)
  - [`tairix-netconfig`](./lib/netconfig.md)
  - [`tairix-path`](./lib/path.md)
  - [`tairix-resref`](./lib/resref.md)
  - [`tairix-rng`](./lib/rng.md)
  - [`tairix-rt` I/O](./lib/rt-io.md)
  - [`tairix-sandbox`](./lib/sandbox.md)
  - [`tairix-supervisor`](./lib/supervisor.md)
  - [`tairix-sysconfig`](./lib/sysconfig.md)
  - [`tairix-termcap`](./lib/termcap.md)
  - [`tairix-usb`](./lib/usb.md)
  - [`tairix-users`](./lib/users.md)
  - [`tairix-util`](./lib/util.md)
  - [`tairix-virtio-input`](./lib/virtio_input.md)
  - [`tairix-vt`](./lib/vt.md)

# Drivers

- [Overview](./drivers/overview.md)
- [Userland driver host](./drivers/host.md)
- [Hardware detection and autoload](./drivers/hardware-detection.md)
- [Driver lifecycle](./drivers/lifecycle.md)
- [Bus drivers](./drivers/bus.md)
- [Virtio transport](./drivers/virtio.md)
- [Block drivers](./drivers/block.md)
- [Network drivers](./drivers/network.md)
- [Display drivers](./drivers/display.md)
- [Input drivers](./drivers/input.md)

# Filesystem

- [Overview](./filesystem/overview.md)
- [Storage namespaces, volume roots, and aliases](./filesystem/drives.md)
- [On-disk layout enforcement](./filesystem/layout.md)
- [Permissions](./filesystem/permissions.md)
- [FAT32 driver](./filesystem/fat32.md)
- [arxfs driver](./filesystem/arxfs.md)
- [arxfs specification](./filesystem/arxfs-spec.md)
- [Extended-metadata preset registry](./filesystem/metadata-registry.md)
- [ext4 driver](./filesystem/ext4.md)
- [ADFS driver](./filesystem/adfs.md)
- [POSIX conformance suite](./filesystem/posix_suite.md)
- [Filesystem soak](./filesystem/soak.md)

# Userland

- [PID 1 service manager](./userland/init.md)
- [System Information service](./userland/sysinfod.md)
- [Seat-manager service](./userland/seatmgr.md)
- [Network-stack service](./userland/netstack.md)
- [Networking tools (`ss`)](./userland/networking.md)
- [elsh (Element Shell)](./userland/shell.md)
- [Text login](./userland/login.md)
- [Application bundle loader](./userland/appmgr.md)
- [Core CLI utilities](./userland/utilities.md)
- [System-log tool (`log`)](./userland/log.md)
- [Building a curses TUI (`top`)](./userland/curses-porting.md)
- [Live kernel monitor (`sysmon`)](./userland/sysmon.md)
- [Load generator (`stress`)](./userland/stress.md)
- [The `vim` editor](./userland/vim.md)
- [The `fstree` file manager](./userland/fstree.md)

# Desktop

- [Compositing window manager](./desktop/wm.md)
- [Seat ownership](./desktop/seat.md)
- [Traditional desktop taskbar](./desktop/taskbar.md)
- [Desktop session glue](./desktop/session.md)
- [Desktop theming](./desktop/theming.md)
- [Pointer cursors](./desktop/cursors.md)
- [Desktop icons](./desktop/icons.md)
- [SVG asset decoding](./desktop/svg-assets.md)
- [Variable DPI and UI scale](./desktop/dpi.md)
- [Default desktop apps](./desktop/apps.md)
- [Widget gallery](./desktop/widgets.md)
- [Design artwork and storyboards](./desktop/artwork.md)

# ABI

- [Driver traits (`abi-v1`)](./abi/driver_traits.md)
- [System Information API (`sysinfo-v1`)](./abi/sysinfo.md)
- [Application bundles (`AppInfo`, `abi-v1`)](./abi/appinfo.md)
- [64-bit-native time (`abi-v1`)](./abi/time.md)
- [Input events (`abi-v1`)](./abi/input.md)
- [Standard Information Stream (`stdinfo`, fd 3)](./abi/stdinfo.md)
- [Datagram socket ABI (`netsock-v1`)](./abi/net-sockets.md)
- [C development header (`abi-v1`)](./abi/c-abi.md)
- [Calling TAIRiX from C (worked example)](./abi/calling-from-c.md)

# Platforms

- [x86_64](./platform/x86_64.md)
- [riscv64](./platform/riscv64.md)
- [aarch64](./platform/aarch64.md)
- [wasm32](./platform/wasm32.md)

# Install

- [Raspberry Pi 4](./install/raspberry_pi.md)
