# Driver traits (`abi-v1`)

This page is the long-form ABI reference for the trait surface in
`lib/abi/src/driver/`. Source-level rustdoc is the authoritative
specification; this page is a guided index.

The driver framework's narrative documentation (lifecycle,
capability model, kinds) lives in
[Driver framework overview](../drivers/overview.md).

## Module layout

| Module                                | Purpose                                |
|---------------------------------------|----------------------------------------|
| [`rustos_abi::driver`]                | Shared types: host, handle, error, kind, manifest. |
| [`rustos_abi::driver::display`]       | `Display` trait + pixel-format types.  |
| [`rustos_abi::driver::filesystem`]    | `Filesystem` trait + mount flags.      |
| [`rustos_abi::driver::block`]         | `Block` trait + geometry.              |
| [`rustos_abi::driver::net`]           | `Net` trait + MAC address.             |
| [`rustos_abi::driver::input`]         | `Input` trait + event records.         |
| [`rustos_abi::driver::bus`]           | `Bus` trait + device records.          |
| [`rustos_abi::driver::virtio_pci`]    | `VirtioPciBus` transport-provisioning seam. |
| [`rustos_abi::driver::virtio_mmio`]   | `VirtioMmioBus` transport-provisioning seam. |

[`rustos_abi::driver`]: #shared-types
[`rustos_abi::driver::display`]: #display
[`rustos_abi::driver::filesystem`]: #filesystem
[`rustos_abi::driver::block`]: #block
[`rustos_abi::driver::net`]: #net
[`rustos_abi::driver::input`]: #input
[`rustos_abi::driver::bus`]: #bus
[`rustos_abi::driver::virtio_pci`]: #virtio-pci-provisioning
[`rustos_abi::driver::virtio_mmio`]: #virtio-mmio-provisioning

## Shared types

### DriverHost

`trait DriverHost` — the host-supplied environment passed to a
driver's `register` entry point.

| Method                              | Capability gate                       |
|-------------------------------------|---------------------------------------|
| `has_capability(cap)`               | None (pure query of load-time grant). |
| `kind()`                            | None.                                 |
| `virtio_host()`                     | None (factory enforces gates).        |

The trait deliberately exposes only what the driver needs to learn
about its own load-time grant. Audit, logging, IPC channels, and
device-tree access live in the userland driver host (delivered as a
separate change set per the Stage 4 task split).

`virtio_host(&self) -> Option<&dyn VirtioHost>` is an `abi-v1`
*internal* extension added at Stage 4.D Item 0-tail. A virtio-class
driver consults it from inside `register()` to obtain the
per-driver host minted by `HostConfig::virtio_host_factory` (see
[Userland driver host](../drivers/host.md#virtio-host-factory)); the
default impl returns `None`, which keeps every existing
non-virtio host source-compatible. The public `register(host:
&dyn DriverHost) -> Result<DriverHandle, DriverError>` entry point
per `AGENTS.md` §8 is unchanged: the accessor takes `&self` to
compose with the immutable driver-host loan, and `VirtioHost`'s
own methods use interior mutability for state. The owned
`DmaSlab`, `PoolId`, and `SlabFreeFn` types backing the trait now
live in `rustos_abi::driver::dma`; `drivers/bus/virtio`
re-exports them so existing import sites remain unchanged.

### DriverHandle

`#[repr(transparent)] struct DriverHandle(u64)`. Issued by the host
to every successfully-loaded driver. The all-zero value is reserved
(`DriverHandle::NONE`) and rejected by
`DriverHandle::from_raw`.

### DriverKind

`#[repr(u8)] enum DriverKind { UserSpace = 0, InKernel = 1 }`.
`InKernel` requires `CAP_DRV_KERNEL` in addition to the universal
`CAP_DRV_LOAD`.

### DriverError

`#[repr(i32)] #[non_exhaustive] enum DriverError`. Twelve variants
covering every failure path the trait surface can produce. Numeric
values are frozen at `abi-v1` and disjoint from
[`rustos_abi::Errno`](../lib/abi.md); use
`DriverError::as_errno` when bridging into a syscall result.

### DriverManifest

`#[repr(C)] struct DriverManifest`. The signed prefix of a driver
module's `rxe` manifest. Wire encoding mirrors
[`rustos_abi::ManifestHeader`](../lib/abi.md) so a single verifier
serves both surfaces. Encoded length is `DriverManifest::WIRE_LEN`
bytes; the signature byte range is the tail of that buffer
(`DriverManifest::signed_range()` returns everything *before* the
signature).

`DriverManifest::from_bytes` rejects:

* short buffers (`DriverError::BufferTooSmall`),
* wrong magic / non-zero reserved bytes (`DriverError::BadMagic`),
* unsupported ABI versions (`DriverError::AbiVersionUnsupported`),
* oversize capability counts (`DriverError::LengthOutOfRange`),
* unknown `kind` byte (`DriverError::OutOfRange`).

## Display

`trait Display`. Methods:

| Method        | Returns                            | Capability gate                       |
|---------------|------------------------------------|---------------------------------------|
| `mode_info()` | `Result<DisplayMode, DriverError>` | Driver handle.                        |
| `present(&)`  | `Result<(), DriverError>`          | Driver handle.                        |

`DisplayMode` carries `width_px`, `height_px`, `stride_bytes`, and
a `DisplayFormat` (`Rgba8888` or `Bgra8888`).

## Filesystem

`trait Filesystem`. Methods:

| Method         | Capability gate                                      |
|----------------|------------------------------------------------------|
| `mount(src, flags)` | `CAP_FS_MOUNT` (plus driver handle).            |
| `unmount()`         | `CAP_FS_MOUNT` (plus driver handle).            |

`MountFlags` is a checked bitmap of `READ_ONLY`, `NOSUID`, `NODEV`,
`NOEXEC`. The installer's secure default layout (`AGENTS.md` §11.3,
§16.3) sets `NOSUID | NODEV` on `/Users` and `/Apps` via this type.

`Filesystem` carries only mount/unmount and a `DriverHandle`, so it
cannot perform path I/O. The read surface the VFS uses to delegate
path resolution to a driver is therefore a **separate versioned
trait**, `FilesystemRead`, not an added method on the frozen one
(`AGENTS.md` §2.4 / §9 — same discipline as the `PortIo8` split):

| Method                            | Capability gate                       |
|-----------------------------------|---------------------------------------|
| `root()`                          | Driver handle.                        |
| `node_info(node)`                 | Driver handle.                        |
| `lookup(dir, name)`               | Driver handle.                        |
| `read_at(file, offset, &mut)`     | Driver handle.                        |
| `read_dir(dir, index, &mut name)` | Driver handle.                        |

The surface is allocation-free: a `NodeId` is an opaque,
implementation-minted token (`NodeId::NONE` is reserved), `NodeInfo`
reports `{ kind, size }`, and `read_dir` writes the entry name into a
caller-provided buffer alongside a `DirEntry { node, kind, name_len }`.
Implementations expose raw structural access only and make **no**
permission decisions — the VFS authorises every traversal against the
§5.3 model before calling here (`AGENTS.md` §5.4). The first
implementation is the [FAT32 driver](../filesystem/fat32.md).

The mutating surface is a second separate versioned trait,
`FilesystemWrite`, again additive rather than a widening of the frozen
`Filesystem` or of `FilesystemRead` (`AGENTS.md` §2.4 / §9):

| Method                                | Capability gate                   |
|---------------------------------------|-----------------------------------|
| `create(dir, name, kind)`             | Driver handle (writable mount).   |
| `write_at(dir, name, offset, data)`   | Driver handle (writable mount).   |
| `truncate(dir, name, size)`           | Driver handle (writable mount).   |
| `remove(dir, name)`                   | Driver handle (writable mount).   |
| `flush()`                             | Driver handle.                    |

Unlike the read surface, the mutating methods address their target as a
`(dir, name)` pair rather than by a `NodeId`: a `NodeId` is
self-describing but carries no back-pointer to the directory entry that
stores a file's length and starting location, and filesystems such as
FAT keep that metadata *in the parent directory*. As on the read side,
the driver makes no permission decision; the VFS authorises the write
against the §5.3 template (and refuses a write to a `READ_ONLY` mount)
before delegating (`AGENTS.md` §5.4). The first implementation is the
read/write [FAT32 driver](../filesystem/fat32.md).

## Block

`trait Block`. Methods:

| Method                  | Capability gate         |
|-------------------------|-------------------------|
| `geometry()`            | Driver handle.          |
| `read_blocks(lba, &mut)`| Driver handle.          |
| `write_blocks(lba, &)`  | Driver handle.          |

Buffer sizes must be a positive integer multiple of
`geometry()?.block_size`; LBA ranges are bounds-checked against
`geometry()?.block_count`.

## Net

`trait Net`. Methods:

| Method               | Capability gate                  |
|----------------------|----------------------------------|
| `mac_address()`      | Driver handle.                   |
| `transmit(&)`        | `CAP_NET_RAW` (plus handle).     |
| `receive(&mut)`      | `CAP_NET_RAW` (plus handle).     |

## Input

`trait Input`. One method:

| Method            | Capability gate    |
|-------------------|--------------------|
| `poll(&mut)`      | Driver handle.     |

Event records are `InputEvent { kind, reserved0, code, value }`;
`InputEventKind` is `Key`, `Pointer`, or `Scroll`.

Legacy x86 input controllers are byte-addressed, so the
`rustos_abi::driver::port_io` module ships an 8-bit port-access seam,
`trait PortIo8 { read8, write8 }`, alongside the existing 32-bit
`PortIo` (which is reserved for PCI mechanism #1 configuration access).
`PortIo` is frozen, so per
`AGENTS.md` §2.4 the byte width is a separate versioned trait rather
than an added method. A driver names `PortIo8` without depending on an
architecture port; the x86_64 port supplies the only real
implementation (`AGENTS.md` §17.2 / §17.4). See
[Input drivers](../drivers/input.md).

## Bus

`trait Bus`. One method:

| Method                | Capability gate     |
|-----------------------|---------------------|
| `enumerate(&mut)`     | Driver handle.      |

`BusDevice` carries `vendor`, `device`, `class`, and bus-local
`address`.

## Virtio-PCI provisioning

`trait VirtioPciBus: Bus`. The boot-time PCI walk that turns a modern
virtio device's vendor-specific capabilities into kernel-mapped
register windows lives in ring 0, but ring 0 may not name a concrete
`drivers/bus/*` type (`AGENTS.md` §8). This trait is the frozen seam
that lets it call into the PCI driver through `&dyn VirtioPciBus`:

| Method                                       | Capability gate                       |
|----------------------------------------------|---------------------------------------|
| `map_virtio_window(bdf, cfg_type, mapper)`   | `CAP_MMIO_MAP` (enforced by `mapper`). |
| `notify_off_multiplier(bdf)`                 | Driver handle.                        |

The `VIRTIO_PCI_CFG_*` constants name the four `cfg_type` discriminants
(common `1`, notify `2`, ISR `3`, device `4`) the caller maps, and
`VIRTIO_PCI_VENDOR_ID` (`0x1AF4`) identifies a virtio function. The
kernel's `provision_virtio_pci` walk (see
[Bus drivers](../drivers/bus.md#ring-0-virtio-pci-walk)) enumerates
through the `Bus` supertrait, picks the matching function, maps the
four windows through the `CAP_MMIO_MAP`-gated `MmioMapper`, and hands
the assembled `PciTransportWindows` (a `lib/virtio` type) to a
caller-supplied builder — so it names neither the concrete `Pci` type
nor the `PciTransport` it produces.

## Virtio-MMIO provisioning

`trait VirtioMmioBus: Bus`. The `virt`-platform counterpart of
`VirtioPciBus`. A virtio-MMIO device is presented as a single
memory-mapped register block discovered from the flat device tree;
the boot-time walk that maps it lives in ring 0, which may not name a
concrete `drivers/bus/*` type (`AGENTS.md` §8). This frozen seam lets
it call into the MMIO bus driver through `&dyn VirtioMmioBus`:

| Method                          | Capability gate                       |
|---------------------------------|---------------------------------------|
| `map_slot_window(base, mapper)` | `CAP_MMIO_MAP` (enforced by `mapper`). |

Unlike the PCI transport's four capability-selected windows, the MMIO
transport consumes exactly one window over the register block at the
slot's `BusDevice::address`. The kernel's `provision_virtio_mmio` walk
(see [Bus drivers](../drivers/bus.md#ring-0-virtio-mmio-walk))
enumerates through the `Bus` supertrait, picks the slot whose
`DeviceID` matches, maps its window through the `CAP_MMIO_MAP`-gated
`MmioMapper`, and hands the window to a caller-supplied builder — so it
names neither the concrete `Mmio` type nor the `MmioTransport` it
produces.

## Versioning

The driver trait surface is part of the frozen `abi-v1` contract.
Adding a method to any of the class traits, or adding a variant to
any of the `#[non_exhaustive]` enums above, is **not** an
in-place change — it requires a new versioned trait/enum under an
`abi-v2` module (`AGENTS.md` §2.4 / §9). `cargo xtask abi-check`
continues to be the cross-check authority for the syscall table; a
parallel guard for the driver surface lands with the userland
driver host change set.
