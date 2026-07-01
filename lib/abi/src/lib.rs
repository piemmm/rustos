//! Stable RustOS user/kernel ABI types.
//!
//! This crate is the single source of truth for the binary interface between
//! the kernel and user space. Every public item is `#[repr(C)]` (or
//! `#[repr(transparent)]` over a `#[repr(C)]` type) with an explicit primitive
//! representation, and the wire layout shipped under [`ABI_VERSION_V1`] is
//! frozen for the lifetime of `abi-v1`: new behaviour ships in `abi-v2`
//! instead of mutating these types in place ().
//!
//! The crate is `no_std`, has no transitive dependencies, and performs no
//! allocation. Encoding and decoding helpers operate exclusively on borrowed
//! byte slices so that they can run inside the kernel, inside a freestanding
//! driver, and inside a WebAssembly userland binary unchanged.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod appinfo;
pub mod boot;
pub mod capability;
pub mod driver;
pub mod driver_store;
pub mod error;
pub mod fs;
pub mod hwtree;
pub mod input;
pub mod ipc;
pub(crate) mod le;
pub mod log;
pub mod mailbox_ipc;
pub mod manifest;
pub mod memory;
pub mod origin;
pub mod process;
pub mod random;
pub mod rlimit;
pub mod rxe;
pub mod stdinfo;
pub mod syscall;
pub mod syscalls;
pub mod sysinfo;
pub mod time;
pub mod usb_urb;
pub mod waitset;

pub use appinfo::{
    body_len as appinfo_body_len, mime_type_at, resolve_library, validate_bundle_layout,
    AppInfoHeader, BundleEntry, BundleLayoutError, LibraryError, LibraryScope, APPINFO_MAGIC,
    APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX,
    MIME_ENTRY_LEN, MIME_TYPE_MAX, SYSTEM_LIBRARIES_DIR,
};
pub use boot::{BootId, BOOT_ID_HEX_LEN, BOOT_ID_LEN};
pub use capability::{CapabilityId, CapabilityQuery, CAPABILITY_ID_MAX};
pub use driver::{
    decode_bind_keys, BufferClass, Delay, DriverBindKey, DriverError, DriverHandle, DriverHost,
    DriverKind, DriverManifest, DriverRegisterReply, MmioMapError, MmioMapper, MsiMessage, MsixBus,
    PciBus, PortIo, PortIo8, RegisterWindow, VirtioMmioBus, VirtioPciBus, WindowError,
    DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_BIND_KEYS, DRIVER_MANIFEST_MAX_CAPABILITIES,
    DRIVER_REGISTER_REPLY_MAGIC, DRIVER_REGISTER_STATUS_OK, DRIVER_SIGNATURE_LEN,
    DRIVER_SIGNER_PUBKEY_LEN, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR,
    VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID,
};
pub use error::Errno;
pub use fs::{DirEntry, FileKind, FileStat, OpenFlags, FS_IO_MAX, FS_NAME_MAX, FS_PATH_MAX};
pub use hwtree::{
    HwDeviceClass, HwMatchKey, HwMatchKind, HwNode, HwResource, HwResourceKind, HwTreeHeader,
    MsiAllocation, HWTREE_VERSION_V1, HW_COMPATIBLE_MAX, HW_NODE_MAX_MATCH_KEYS,
    HW_NODE_MAX_RESOURCES, HW_NODE_ROOT, HW_NODE_ROOT_ID,
};
pub use input::{
    KeyInput, KeyValue, Modifiers, NamedKeyCode, PointerButtonCode, PointerInput, BUTTON_NONE,
    KEY_CLASS_CHAR, KEY_CLASS_NAMED, KEY_INPUT_MAGIC, KIND_KEY_PRESSED, KIND_KEY_RELEASED,
    KIND_MOVED, KIND_PRESSED, KIND_RELEASED, MOD_ALT, MOD_CTRL, MOD_MASK, MOD_META, MOD_SHIFT,
    POINTER_INPUT_MAGIC,
};
pub use ipc::{IpcMessageHeader, PortName, IPC_MESSAGE_HEADER_MAGIC, PORT_NAME_MAX_LEN};
pub use log::{
    decode_record as decode_log_record, encode_record as encode_log_record, LogFieldIter,
    LogRecordRef, LOG_FIELDS_MAX, LOG_FIELD_KEY_MAX, LOG_FIELD_VALUE_MAX, LOG_LEVEL_MAX,
    LOG_MESSAGE_MAX, LOG_RECORD_HEADER_LEN, LOG_RECORD_MAX,
};
pub use manifest::{
    decode_capability_ids, ManifestHeader, MANIFEST_MAGIC, MANIFEST_MAX_CAPABILITIES,
};
pub use memory::MapFlags;
pub use origin::{
    CapabilitySummary, Origin, ProcId, TrustDomain, CAPABILITY_SUMMARY_LEN, ORIGIN_WIRE_LEN,
    PROC_ID_HEX_LEN, PROC_ID_LEN,
};
pub use process::{
    encoded_len as process_start_encoded_len, write_into as process_start_write_into,
    DescriptorTable, ProcessStart, ProcessStartHeader, StreamMode, StringSlot, CONSOLE_INDEX_MAX,
    CONSOLE_INHERIT, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS, PROCESS_START_MAX_STRING_LEN,
    PROCESS_START_MAX_TOTAL_LEN, STDERR, STDIN, STDINFO, STDOUT, STD_STREAM_COUNT,
};
pub use random::{RandomFlags, RANDOM_REQUEST_MAX_BYTES, RANDOM_RESERVE_DEFAULT_BYTES};
pub use rlimit::{LimitKind, ResourceLimit, RLIMIT_INFINITY};
pub use rxe::{
    kaslr_bias, LoadHeader, LoadImage, NeededLibrary, RxeError, RxePermission, Segment, LIBREF_MAX,
    LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_NEEDED, LOAD_MAX_SEGMENTS, RXE_PAGE_SIZE, SEG_FLAG_EXEC,
    SEG_FLAG_READ, SEG_FLAG_WRITE,
};
pub use stdinfo::{
    Human, Severity, StdInfoKind, StdInfoRecord, STDINFO_FD, STDINFO_VERSION_CURRENT,
    STDINFO_VERSION_V1,
};
pub use syscall::{IrqHandle, SyscallNumber, SYSCALL_TABLE_HASH_LEN, WAIT_PID_ANY};
pub use syscalls::{
    encoded_table, spec_for, AbiType, SyscallSpec, ENCODED_TABLE, ENCODED_TABLE_LEN, SYSCALLS,
    SYSCALL_ENCODED_RECORD_LEN, SYSCALL_MAX_ARGS, SYSCALL_NAME_MAX,
};
pub use sysinfo::{
    encoded_query_table, spec_for as sysinfo_spec_for, KernelMemoryStats, MountListRequest,
    MountRecord, ProcessListRequest, ProcessRecord, ProcessState, ResourceLimitRecord,
    SysinfoQueryId, SysinfoQuerySpec, SysinfoRequestHeader, SystemIdentity, Uptime,
    ENCODED_QUERY_TABLE, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX, MACHINE_ID_LEN, MOUNT_FSTYPE_MAX,
    MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, PROCESS_NAME_MAX, RESOURCE_LIMITS_REPORT_LEN,
    SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERIES, SYSINFO_QUERY_NAME_MAX, SYSINFO_QUERY_RECORD_LEN,
    SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1,
};
pub use time::{
    coarsen_clock_ns, Duration64, Time64, WallClockReading, WallTimeState,
    COARSE_CLOCK_GRANULARITY_NS, NANOS_PER_SEC,
};
pub use waitset::{WaitSetOp, WaitSourceKind};

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

/// [`ABI_VERSION_CURRENT`] as the `u16` carried by the wire formats whose
/// version field is two bytes wide (for example [`ipc::IpcMessageHeader`] and
/// [`input::PointerInput`]). Defined once so an encoder never open-codes a
/// truncating `as u16` cast at the call site.
// `ABI_VERSION_V1` is 1 and every supported ABI version fits in a `u16`; the
// narrowing is exact, and a future version that did not fit would be a
// deliberate ABI decision made here, not a silent truncation.
#[allow(clippy::cast_possible_truncation)]
pub const ABI_VERSION_CURRENT_U16: u16 = ABI_VERSION_V1 as u16;

/// Result alias used throughout the ABI surface.
///
/// All fallible ABI helpers return [`Errno`] on failure. The alias exists so
/// that downstream crates do not name `core::result::Result` with two type
/// parameters at every call site.
pub type Result<T> = core::result::Result<T, Errno>;
