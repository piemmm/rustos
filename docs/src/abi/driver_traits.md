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
| [`rustos_abi::driver::pci`]           | `PciBus` generic BAR / bus-master seam. |

[`rustos_abi::driver`]: #shared-types
[`rustos_abi::driver::display`]: #display
[`rustos_abi::driver::filesystem`]: #filesystem
[`rustos_abi::driver::block`]: #block
[`rustos_abi::driver::net`]: #net
[`rustos_abi::driver::input`]: #input
[`rustos_abi::driver::bus`]: #bus
[`rustos_abi::driver::virtio_pci`]: #virtio-pci-provisioning
[`rustos_abi::driver::virtio_mmio`]: #virtio-mmio-provisioning
[`rustos_abi::driver::pci`]: #generic-pci-provisioning

## Shared types

### DriverHost

`trait DriverHost` — the host-supplied environment passed to a
driver's `register` entry point.

| Method                              | Capability gate                       |
|-------------------------------------|---------------------------------------|
| `has_capability(cap)`               | None (pure query of load-time grant). |
| `kind()`                            | None.                                 |
| `virtio_host()`                     | None (host enforces `CAP_MEM_DMA` per alloc). |
| `mmio_mapper()`                     | None (mapper enforces `CAP_MMIO_MAP` per map). |
| `dma_host()`                        | None (host enforces `CAP_MEM_DMA` per alloc). |
| `mailbox()`                         | None (host enforces the doorbell/buffer gate). |
| `emit_node(node)`                   | None at the call site (host gates tree mutation). |

The trait deliberately exposes only what the driver needs to learn
about its own load-time grant and to reach the host facilities its
matched node was granted. Audit, logging, and IPC channels live in the
userland driver host. Every accessor that returns `Option`/`Result`
reports an *absent* facility as `None` / `DriverError::Unsupported` and
never as a synthesised handle, so a bus driver fails closed when a
facility it needs is not wired (`AGENTS.md` §18.4 / §5.4).

`dma_host(&self) -> Option<&dyn DmaHost>` is the **bus-neutral**
DMA-allocation accessor, a sibling of `mmio_mapper()`. A non-virtio bus
driver — e.g. the floor xHCI bring-up staging device-context and ring
memory — allocates a `DmaSlab` through it without reaching through the
virtio-shaped `VirtioHost`. The allocation contract lives once in
`trait DmaHost { fn alloc_dma_zeroed(&self, size) -> Result<DmaSlab,
DriverError>; }`; `VirtioHost: DmaHost` extends it, so a virtio host is
also a DMA host and the contract is never duplicated (`AGENTS.md` §2.2).

`mailbox(&self) -> Option<&dyn MailboxChannel>` is the board-neutral
firmware property-mailbox seam. A bus driver whose bring-up needs the
platform firmware (the BCM2711 VideoCore reload of the VL805 USB
controller firmware) marshals 32-word property messages
(`MAILBOX_PROPERTY_WORDS`) through it; the doorbell window, the
DMA-backed property buffer, and the bus-address translation stay behind
the host and the device's own `lib/vcmailbox` crate (`AGENTS.md` §2.20),
so the generic framework above it never names a board.

`emit_node(&self, node: HwNode) -> Result<(), DriverError>` lets a bus
driver publish a *discovered child* node into the hardware tree
(`AGENTS.md` §18.1): the enumerated device, carrying the `HwResource`
grant **requests** (register window + DMA region) the matched downstream
driver will receive. The §18.3 match/autoload path then sees a bindable
node like any other discovered device, and the driver requests only what
its enumeration found — no ambient authority (`AGENTS.md` §4).

These four facility accessors are `abi-v1` *internal* additions: like
`virtio_host()` before them, each carries a default body so every
existing host impl stays source-compatible, and the public `register`
entry point is unchanged. They are the host surface the **autonomous
floor bring-up** consumes (see below).

#### Autonomous floor bring-up entry

`register(host)` is *reactive* — the host calls it to instantiate a
driver against an already-discovered node. The bootstrap-floor bus
chain, by contrast, must run **before** any node for the devices behind
it exists: it has to train the PCIe root complex, reload the device's
firmware over the mailbox, bring up the xHCI controller, enumerate the
boot device, and only then `emit_node()` the discovered children. That
work is exposed as a distinct, documented floor entry point a compiled-in
floor driver provides and the kernel's bootstrap-floor catalogue
(`AGENTS.md` §18.6) drives directly, talking to the kernel solely through
this `DriverHost` contract (no `kernel/*` dependency — `AGENTS.md` §17.4).
The autonomous entry consumes `mmio_mapper()` (map the discovered
register window), `dma_host()` (stage controller DMA), `mailbox()`
(firmware reload), and `emit_node()` (publish the enumerated child),
keeping the floor driver free of ambient authority and of any board
name in the generic layers above it.

The chain is split across **two** floor crates, each strictly its own
device, with no driver naming another (`AGENTS.md` §8 / §17.4): the
board-specific PCIe / firmware steps live in the device's own
`drivers/bus/pcie_brcm` (the §2.20 carve-out that may know the BCM2711 /
VL805 — including the firmware reload, which is a VL805-specific mailbox
operation and must **not** leak into the generic USB layer), while the
board-neutral xHCI bring-up, enumeration, and HID-node emission live in
`drivers/bus/usb`. The kernel's bootstrap-floor catalogue sequences the
two autonomous entries and the hardware tree decouples them, so neither
crate depends on the other.

The board-neutral USB half is the landed
`rustos_drv_bus_usb::wiring::bring_up_boot_input`: it maps the controller
BAR (`mmio_mapper()`), carves the device-shared DMA region (`dma_host()`
— a USB host controller is not virtio, so it uses the bus-neutral DMA
seam, not `virtio_host()`), brings the controller up, enumerates the boot
device, augments the enumerated HID `HwNode` with its xHCI-BAR
(`HwResource::mmio`) and DMA (`HwResource::dma`) grant *requests* — exactly
what the matched user-space `usb_kbd` driver receives, no more
(`AGENTS.md` §4) — and `emit_node()`s it. QEMU models no Pi USB
controller, so its host tests prove the composition and its fail-closed
paths up to the controller hand-off; the live enumerate→emit path is the
on-metal acceptance item.

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
signature). The body that follows the header is the requested
capability list (`capability_count` little-endian `u16` ids) followed
by the driver's bind table (`bind_key_count`
[`DriverBindKey`](#driverbindkey) records) and then the opaque payload
(the program image for a `kind = UserSpace` driver, empty for an
in-kernel one). All three — capability list, bind table, *and* payload —
are covered by the signature, so a spawned driver's program is
authenticated and cannot be substituted after signing (`AGENTS.md` §8 /
§2.17).

`DriverManifest::from_bytes` rejects:

* short buffers (`DriverError::BufferTooSmall`),
* wrong magic (`DriverError::BadMagic`),
* unsupported ABI versions (`DriverError::AbiVersionUnsupported`),
* oversize capability or bind-key counts
  (`DriverError::LengthOutOfRange`),
* unknown `kind` byte (`DriverError::OutOfRange`).

### DriverBindKey

`#[repr(C)] struct DriverBindKey`. One entry of a driver manifest's
bind table (`AGENTS.md` §18.3): a hardware-tree
`rustos_abi::hwtree::HwMatchKey` plus the manifest-declared
bind `priority`. The device manager matches each hardware-tree node's
keys against every driver's bind table; when more than one driver
matches the same node, the higher matched `priority` binds, and an
unbroken tie is a packaging defect refused deterministically. Encoded
length is `DriverBindKey::WIRE_LEN` (80) bytes; a manifest may declare
at most `DRIVER_MANIFEST_MAX_BIND_KEYS` (16) entries — a validation
bound, not a capacity (`AGENTS.md` §24.4).

`DriverBindKey::from_bytes` rejects short buffers
(`DriverError::BufferTooSmall`), a non-zero reserved field
(`DriverError::BadMagic`), an unknown match-key kind
(`DriverError::OutOfRange`), and an out-of-bounds `compatible` length
(`DriverError::LengthOutOfRange`); `decode_bind_keys` is the single
shared decoder for the whole table.

### DriverRegisterReply

`#[repr(C)] struct DriverRegisterReply` (`driver::register`). The
outcome of a spawned driver process's `register()` entry, reported to
the driver host over IPC (`PLAN.md` Stage 4.HW): `status` is
`DRIVER_REGISTER_STATUS_OK` (0) or the `DriverError::as_i32` code the
driver reported, and `handle` carries the driver-reported handle's raw
value exactly when `status` is OK. Encoded length is
`DriverRegisterReply::WIRE_LEN` (24) bytes. The record is
informational only — the host mints its own unforgeable
[`DriverHandle`](#driverhandle) on success, so a forged reply widens no
authority.

`DriverRegisterReply::from_bytes` rejects short buffers, wrong magic /
non-zero reserved words, unsupported ABI versions, unknown `status`
codes, and a `status`/`handle` pair that is inconsistent (a success
with the zero sentinel handle, or a failure carrying a handle) — the
whole record is refused on any failure (fail closed). The spawned
driver builds the record with `registered(handle)` / `failed(error)`
and sends it with the `rustos-rt` `ipc_send` wrapper to the reply
endpoint id its host handed it through its startup arguments
(`rustos_rt::arg`).

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
implementation is the [FAT32 driver](../filesystem/fat32.md); the
read-only [ext4 driver](../filesystem/ext4.md) implements this trait
without `FilesystemWrite`.

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

A driver that stores full POSIX metadata *per inode* — owner, group,
mode bits, an ACL, and an optional capability gate (§5.3) — additionally
implements a third separate versioned trait, `FilesystemSecurity`, so
the VFS can use that **stored** record as the policy input instead of a
uniform mount-point template:

| Method            | Capability gate    |
|-------------------|--------------------|
| `security(node)`  | Driver handle.     |

`security` returns a `NodeSecurity { mode, uid, gid, required_cap, acl }`
record, where `acl` is up to `MAX_ACL_ENTRIES` (eight) grant-only
`SecurityAcl { subject, perms }` entries (`subject` is a
`SecuritySubject::User | Group`, `perms` an `rwx` triad). Like the other
surfaces the driver only *reports* the record and makes no decision; the
VFS translates it into its policy metadata and applies the §5.3 model
(`AGENTS.md` §5.4). A driver such as FAT that keeps no per-file owner
simply does not implement this trait, and the VFS keeps applying the
mount-point template. The first implementation is the native
[`rustfs` driver](../filesystem/rustfs.md); the
[`ext4` driver](../filesystem/ext4.md) also implements it, reporting each
inode's stored mode and owner (its POSIX ACLs live in xattr blocks the
read surface does not yet decode, so it surfaces no `required_cap` and no
ACL entries).

A driver whose on-disk format stores the four §21 timestamps as true
`Time64` additionally implements a fourth separate versioned trait,
`FilesystemTimestamps`, alongside (never a widening of) the others
(`AGENTS.md` §2.4 / §9 / §21):

| Method         | Capability gate    |
|----------------|--------------------|
| `times(node)`  | Driver handle.     |

`times` returns a `NodeTimes { created, modified, accessed, changed }`
record, each field a 64-bit-native `Time64` (signed seconds plus
nanoseconds), so absolute time is never a seconds-only scalar and the
full pre-1970 / post-2038 range round-trips without truncation. A driver
whose backing format keeps no timestamps (or only narrower legacy ones it
cannot widen) simply does not implement it. The first implementation is
the native [`rustfs` driver](../filesystem/rustfs.md).

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

The module also carries `trait ReportSource { next_report(&mut, &mut
[u8]) }` — the HID report-delivery seam between the bus driver that
services a device's interrupt-IN endpoint (`drivers/bus/usb`) and the
input decoder that turns reports into events
(`drivers/input/usb_hid`). It lives here because its two sides are
sibling drivers and drivers depend only on `lib/*` (`AGENTS.md`
§17.4). A source must never claim more bytes than the caller's buffer
holds; consumers reject such a claim as a `DeviceFault` (`AGENTS.md`
§5.4).

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

## Generic-PCI provisioning

`trait PciBus: Bus`. A non-virtio, DMA-driving PCI device — the
Raspberry Pi 4's VL805 `PCIe` xHCI USB host controller — exposes its
registers as a whole BAR and needs no MSI-X, so it consumes a smaller
seam than `VirtioPciBus`. A device-class driver (`drivers/bus/usb`)
reaches the PCI driver through `&dyn PciBus`, never naming the concrete
`Pci` type (`AGENTS.md` §8 / §17.4):

| Method                                | Capability gate                        |
|---------------------------------------|----------------------------------------|
| `map_bar_window(bdf, bar_index, mapper)` | `CAP_MMIO_MAP` (enforced by `mapper`). |
| `enable_bus_master(bdf)`              | Driver handle.                         |

`map_bar_window` resolves the memory BAR's probed base/length and maps
it through the `CAP_MMIO_MAP`-gated `MmioMapper` (refusing I/O-port and
unused BARs); `enable_bus_master` sets the function's Memory Space + Bus
Master Enable bits so the controller may issue upstream DMA. `Pci<C>`
implements it by forwarding to the inherent methods, sharing the
bus-master activation with `route_msix` (`AGENTS.md` §2.2). The xHCI
bring-up consumes it in `rustos_drv_bus_usb::wiring::open_discovered`
(see [Bus drivers](../drivers/bus.md#generic-pci-bar-hand-off-the-xhci--vl805-path)).

## Versioning

The driver trait surface is part of the frozen `abi-v1` contract.
Adding a method to any of the class traits, or adding a variant to
any of the `#[non_exhaustive]` enums above, is **not** an
in-place change — it requires a new versioned trait/enum under an
`abi-v2` module (`AGENTS.md` §2.4 / §9). `cargo xtask abi-check`
continues to be the cross-check authority for the syscall table; a
parallel guard for the driver surface lands with the userland
driver host change set.
