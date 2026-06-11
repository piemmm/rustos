# Driver framework overview

This page is the orientation document for Stage 4 of `PLAN.md`. It
describes the trait surface every RustOS driver implements, the
lifecycle the host walks each driver through, and the capability
model that gates every method.

The traits themselves live in
[`rustos_abi::driver`](../abi/driver_traits.md) and are part of the
frozen `abi-v1` contract (`AGENTS.md` §9). New methods do **not**
ship as additions to an existing trait — that would be interface
creep, forbidden by `AGENTS.md` §2.4. Behaviour beyond what the
current traits express is introduced as a new versioned trait in
`abi-v2`.

## Trait surface

Six driver-class traits are defined under
`lib/abi/src/driver/`:

| Class      | Trait                                       | Stage 4 first drivers                     |
|------------|---------------------------------------------|-------------------------------------------|
| Display    | [`driver::display::Display`]                | `drivers/display/{vesa,framebuffer,gpu_virtio}` |
| Filesystem | [`driver::filesystem::Filesystem`]          | `drivers/filesystem/{fat32,ext4,rustfs}` (Stage 5) |
| Block      | [`driver::block::Block`]                    | `drivers/storage/virtio_blk`              |
| Net        | [`driver::net::Net`]                        | `drivers/network/virtio_net`              |
| Input      | [`driver::input::Input`]                    | `drivers/input/{ps2,usb_hid}`             |
| Bus        | [`driver::bus::Bus`]                        | `drivers/bus/{pci,mmio,virtio}`           |

Each class crate (`drivers/<class>/<name>/`) ships exactly one
`pub fn register(host: &dyn DriverHost) -> Result<DriverHandle,
DriverError>` entry point and the trait `impl` for its class
(`AGENTS.md` §8).

## Driver kinds

The [`DriverKind`] declared by a driver's signed manifest is the
single switch that selects between the two supported execution
environments:

* `UserSpace` (the default per `AGENTS.md` §8) — the driver image
  loads into an isolated user-space process. Hardware access flows
  through capability-checked IPC.
* `InKernel` — the driver image is linked into the kernel address
  space and may touch MMIO directly. Loading requires both
  `CAP_DRV_LOAD` and `CAP_DRV_KERNEL`.

## Capability model

Capabilities are enforced **at the dispatch site**, not inside the
trait implementation (`AGENTS.md` §5.2). Three layers compose:

1. **Load-time grants.** The host computes the intersection of the
   driver's manifest request and the loader's own capability set.
   The driver receives a [`DriverHandle`] only if the intersection
   covers every capability the manifest requests.
2. **Per-method gates.** Class methods that need an extra capability
   document it in their rustdoc `# Capabilities` section. The host
   re-checks that capability on every call before dispatching into
   the trait body.
3. **Handle ownership.** Every method requires the caller to present
   the [`DriverHandle`] returned by `register`. The handle is itself
   the kernel-issued proof that the load-time check passed.

The capability identifiers used by the trait surface are all
defined in [`rustos_abi::CapabilityId`] and are frozen at `abi-v1`
(`AGENTS.md` §5.2).

## Lifecycle

A driver passes through four states observable to the host:

1. **Load.** The host verifies the manifest signature
   (`AGENTS.md` §9), validates capability requests against the
   loader's grant, and refuses on mismatch with
   [`DriverError::PermissionDenied`]. Manifest parsing happens via
   [`DriverManifest::from_bytes`].
2. **Init.** The host invokes the driver's `register` entry point,
   passing a `&dyn DriverHost`. On success the driver returns a
   [`DriverHandle`] the host stores in its driver table.
3. **Run.** The host dispatches class-trait methods on behalf of
   the in-kernel and user-space callers it serves, re-checking the
   per-method capability gate on every call.
4. **Unload.** Dropping the [`DriverHandle`] is the canonical way
   to unload a driver. The driver's `Drop` impl quiesces hardware
   and releases capabilities. Forcible unload (`CAP_DRV_LOAD`
   holder, hot-unplug) calls the same path.

## Autoload

Which driver gets loaded for which detected device is decided by the
user-space device manager (`userland/system/devmgr`): it matches each
hardware-tree node's keys against the bind table in every driver's
signed manifest and drives the winners through the host's load gate.
See [hardware detection and autoload](./hardware-detection.md).

## Error surface

[`DriverError`] is the single error type returned across the driver
ABI. The variants are kept disjoint from
[`rustos_abi::Errno`] so that a stray driver error cannot be
confused with a kernel error by the dispatcher; the
`DriverError::as_errno` mapping is used when a driver outcome is
surfaced through a syscall.

[`driver::display::Display`]: ../abi/driver_traits.md#display
[`driver::filesystem::Filesystem`]: ../abi/driver_traits.md#filesystem
[`driver::block::Block`]: ../abi/driver_traits.md#block
[`driver::net::Net`]: ../abi/driver_traits.md#net
[`driver::input::Input`]: ../abi/driver_traits.md#input
[`driver::bus::Bus`]: ../abi/driver_traits.md#bus
[`DriverKind`]: ../abi/driver_traits.md#driverkind
[`DriverHandle`]: ../abi/driver_traits.md#driverhandle
[`DriverError`]: ../abi/driver_traits.md#drivererror
[`DriverError::PermissionDenied`]: ../abi/driver_traits.md#drivererror
[`DriverManifest::from_bytes`]: ../abi/driver_traits.md#drivermanifest
[`rustos_abi::Errno`]: ../lib/abi.md
[`rustos_abi::CapabilityId`]: ../lib/abi.md
