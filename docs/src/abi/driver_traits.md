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

[`rustos_abi::driver`]: #shared-types
[`rustos_abi::driver::display`]: #display
[`rustos_abi::driver::filesystem`]: #filesystem
[`rustos_abi::driver::block`]: #block
[`rustos_abi::driver::net`]: #net
[`rustos_abi::driver::input`]: #input
[`rustos_abi::driver::bus`]: #bus

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

## Bus

`trait Bus`. One method:

| Method                | Capability gate     |
|-----------------------|---------------------|
| `enumerate(&mut)`     | Driver handle.      |

`BusDevice` carries `vendor`, `device`, `class`, and bus-local
`address`.

## Versioning

The driver trait surface is part of the frozen `abi-v1` contract.
Adding a method to any of the class traits, or adding a variant to
any of the `#[non_exhaustive]` enums above, is **not** an
in-place change — it requires a new versioned trait/enum under an
`abi-v2` module (`AGENTS.md` §2.4 / §9). `cargo xtask abi-check`
continues to be the cross-check authority for the syscall table; a
parallel guard for the driver surface lands with the userland
driver host change set.
