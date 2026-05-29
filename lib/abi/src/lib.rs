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
pub mod manifest;
pub mod syscall;
pub mod syscalls;

pub use capability::{CapabilityId, CAPABILITY_ID_MAX};
pub use driver::{
    BufferClass, DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest,
    DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_CAPABILITIES, DRIVER_SIGNATURE_LEN,
    DRIVER_SIGNER_PUBKEY_LEN,
};
pub use error::Errno;
pub use ipc::{IpcMessageHeader, IPC_MESSAGE_HEADER_MAGIC};
pub use manifest::{ManifestHeader, MANIFEST_MAGIC, MANIFEST_MAX_CAPABILITIES};
pub use syscall::{IrqHandle, SyscallNumber, SYSCALL_TABLE_HASH_LEN};
pub use syscalls::{
    encoded_table, spec_for, AbiType, SyscallSpec, ENCODED_TABLE, ENCODED_TABLE_LEN, SYSCALLS,
    SYSCALL_ENCODED_RECORD_LEN, SYSCALL_MAX_ARGS, SYSCALL_NAME_MAX,
};

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
