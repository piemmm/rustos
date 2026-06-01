//! Stable RustOS user/kernel ABI types.
//!
//! This crate is the single source of truth for the binary interface between
//! the kernel and user space. Every public item is `#[repr(C)]` (or
//! `#[repr(transparent)]` over a `#[repr(C)]` type) with an explicit primitive
//! representation, and the wire layout shipped under [`ABI_VERSION_V1`] is
//! frozen for the lifetime of `abi-v1`: new behaviour ships in `abi-v2`
//! instead of mutating these types in place (see `AGENTS.md` §9).
//!
//! The crate is `no_std`, has no transitive dependencies, and performs no
//! allocation. Encoding and decoding helpers operate exclusively on borrowed
//! byte slices so that they can run inside the kernel, inside a freestanding
//! driver, and inside a WebAssembly userland binary unchanged.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod capability;
pub mod driver;
pub mod error;
pub mod ipc;
pub(crate) mod le;
pub mod manifest;
pub mod rxe;
pub mod stdinfo;
pub mod syscall;
pub mod syscalls;
pub mod sysinfo;
pub mod time;

pub use capability::{CapabilityId, CapabilityQuery, CAPABILITY_ID_MAX};
pub use driver::{
    BufferClass, DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest, MmioMapError,
    MmioMapper, MsiMessage, MsixBus, PortIo, PortIo8, RegisterWindow, VirtioMmioBus, VirtioPciBus,
    WindowError, DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_CAPABILITIES, DRIVER_SIGNATURE_LEN,
    DRIVER_SIGNER_PUBKEY_LEN, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR,
    VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID,
};
pub use error::Errno;
pub use ipc::{IpcMessageHeader, IPC_MESSAGE_HEADER_MAGIC};
pub use manifest::{
    decode_capability_ids, ManifestHeader, MANIFEST_MAGIC, MANIFEST_MAX_CAPABILITIES,
};
pub use rxe::{
    kaslr_bias, LoadHeader, LoadImage, RxeError, RxePermission, Segment, LOAD_FLAG_PIE, LOAD_MAGIC,
    LOAD_MAX_SEGMENTS, RXE_PAGE_SIZE, SEG_FLAG_EXEC, SEG_FLAG_READ, SEG_FLAG_WRITE,
};
pub use stdinfo::{
    Human, Severity, StdInfoKind, StdInfoRecord, STDINFO_FD, STDINFO_VERSION_CURRENT,
    STDINFO_VERSION_V1,
};
pub use syscall::{IrqHandle, SyscallNumber, SYSCALL_TABLE_HASH_LEN};
pub use syscalls::{
    encoded_table, spec_for, AbiType, SyscallSpec, ENCODED_TABLE, ENCODED_TABLE_LEN, SYSCALLS,
    SYSCALL_ENCODED_RECORD_LEN, SYSCALL_MAX_ARGS, SYSCALL_NAME_MAX,
};
pub use sysinfo::{
    encoded_query_table, spec_for as sysinfo_spec_for, KernelMemoryStats, ProcessListRequest,
    ProcessRecord, ProcessState, SysinfoQueryId, SysinfoQuerySpec, SysinfoRequestHeader,
    SystemIdentity, Uptime, ENCODED_QUERY_TABLE, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX,
    MACHINE_ID_LEN, PROCESS_NAME_MAX, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERIES,
    SYSINFO_QUERY_NAME_MAX, SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC,
    SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1,
};
pub use time::{Duration64, Time64, NANOS_PER_SEC};

/// ABI version tag for the frozen `abi-v1` interface.
///
/// Binaries embed this value in their [`ManifestHeader`] so that a kernel
/// loading them can refuse a manifest produced for a future ABI revision
/// without attempting to interpret its body.
pub const ABI_VERSION_V1: u32 = 1;

/// The current ABI version supported by this crate.
///
/// Equal to [`ABI_VERSION_V1`] today; when `abi-v2` is introduced this
/// constant will be re-pointed and `abi-v1` will move to a compatibility
/// submodule rather than mutate in place.
pub const ABI_VERSION_CURRENT: u32 = ABI_VERSION_V1;

/// Result alias used throughout the ABI surface.
///
/// All fallible ABI helpers return [`Errno`] on failure. The alias exists so
/// that downstream crates do not name `core::result::Result` with two type
/// parameters at every call site.
pub type Result<T> = core::result::Result<T, Errno>;
