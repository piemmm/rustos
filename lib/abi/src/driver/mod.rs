//! Driver class trait surface (Stage 4 of `PLAN.md`).
//!
//! This module is the single source of truth for the user/kernel ABI that
//! sits between a loaded driver module (`.rxe` binary, see `AGENTS.md` §9)
//! and its host. The host lives in user space by default (`AGENTS.md` §4)
//! and dispatches calls into one of the six driver-class traits defined in
//! the submodules below:
//!
//! * [`display`] — framebuffer and GPU surfaces.
//! * [`filesystem`] — mountable filesystems.
//! * [`block`] — block-addressable storage.
//! * [`net`] — link-layer network interfaces.
//! * [`input`] — keyboard / pointer / scroll input.
//! * [`bus`] — bus enumeration (PCI, MMIO, virtio).
//!
//! # Lifecycle
//!
//! A driver progresses through four observable states:
//!
//! 1. **Load** — the host parses the signed [`DriverManifest`] (this
//!    module) and verifies the requested capabilities against the
//!    caller's grant. Mismatch produces [`DriverError::PermissionDenied`].
//! 2. **Init** — the host calls the driver-supplied
//!    `pub fn register(host: &dyn DriverHost) -> Result<DriverHandle,
//!    DriverError>` entry point. The driver returns a
//!    [`DriverHandle`]; that handle is the only public proof of a live
//!    driver instance.
//! 3. **Run** — the host dispatches class-trait methods on behalf of
//!    callers. Every method is capability-gated *at the dispatch
//!    site*; the trait implementation never re-checks
//!    (`AGENTS.md` §5.2).
//! 4. **Unload** — the host drops the [`DriverHandle`]; the driver's
//!    `Drop` impl quiesces hardware and releases capabilities.
//!
//! # Capabilities
//!
//! Loading any driver requires
//! [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD).
//! Loading a driver whose manifest declares
//! [`DriverKind::InKernel`] additionally requires
//! [`CapabilityId::DRV_KERNEL`](crate::CapabilityId::DRV_KERNEL).
//! Class traits document their own per-method capability gates.

use crate::le::{read_u16, read_u32};
use crate::syscall::SYSCALL_TABLE_HASH_LEN;
use crate::{CapabilityId, Errno};

pub mod block;
pub mod bus;
pub mod display;
pub mod dma;
pub mod filesystem;
pub mod input;
pub mod mmio;
pub mod msix;
pub mod net;
pub mod port_io;
pub mod register;
pub mod virtio;
pub mod virtio_mmio;
pub mod virtio_pci;

pub use dma::{DmaSlab, PoolId, SlabFreeFn};
pub use mmio::{MmioMapError, MmioMapper, RegisterWindow, WindowError};
pub use msix::{MsiMessage, MsixBus};
pub use port_io::{PortIo, PortIo8};
pub use register::{DriverRegisterReply, DRIVER_REGISTER_REPLY_MAGIC, DRIVER_REGISTER_STATUS_OK};
pub use virtio::VirtioHost;
pub use virtio_mmio::VirtioMmioBus;
pub use virtio_pci::{
    VirtioPciBus, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR,
    VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID,
};

/// Sensitivity class of a payload buffer crossing the driver ABI.
///
/// Stage 4.D introduced this hint so block- and network-driver
/// implementations can scrub their internal DMA staging buffers as
/// soon as a payload leaves them (`AGENTS.md` §4 "zero-on-free for
/// any allocation that ever held credentials, keys, or capability
/// tokens"). The flag is a *promise about the buffer's contents*,
/// not an access-control gate: capability enforcement remains at the
/// dispatch site (`AGENTS.md` §5.4).
///
/// The variant set is `#[non_exhaustive]` so future classes (for
/// example a `Secret` class that pins the driver's staging into a
/// memory-encryption realm) can be added without an ABI break.
///
/// # Wire form
///
/// On the wire `BufferClass` is a single byte. Hosts that bridge the
/// hint through a syscall must reject unknown values rather than
/// silently downgrading to [`BufferClass::NonSensitive`] (failing
/// closed, `AGENTS.md` §5.4.5).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum BufferClass {
    /// The buffer holds no security-relevant material. Drivers may
    /// retain staging copies until ordinary deallocation.
    NonSensitive = 0,
    /// The buffer held — or may have held — credentials, keys,
    /// session tokens, capability bitmaps, or otherwise security-
    /// relevant payload. Drivers **must** zero every internal copy
    /// of the buffer before returning from the call.
    Sensitive = 1,
}

impl BufferClass {
    /// Raw on-wire byte value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Construct a [`BufferClass`] from its raw on-wire byte.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw` does not name a
    /// known class. The host must fail closed in that case
    /// (`AGENTS.md` §5.4.5).
    ///
    /// # Capabilities
    ///
    /// None.
    pub const fn from_u8(raw: u8) -> Result<Self, DriverError> {
        match raw {
            0 => Ok(Self::NonSensitive),
            1 => Ok(Self::Sensitive),
            _ => Err(DriverError::OutOfRange),
        }
    }

    /// Whether the buffer requires zero-on-free handling.
    #[must_use]
    pub const fn is_sensitive(self) -> bool {
        matches!(self, Self::Sensitive)
    }
}

/// Magic number identifying an `abi-v1` driver manifest
/// (`"DRV1"` little-endian).
pub const DRIVER_MANIFEST_MAGIC: u32 = u32::from_le_bytes(*b"DRV1");

/// Maximum number of capabilities a single driver manifest may request.
///
/// Bounded so that a hostile manifest cannot force unbounded parsing
/// work. The value matches
/// [`MANIFEST_MAX_CAPABILITIES`](crate::MANIFEST_MAX_CAPABILITIES) so
/// driver and application binaries share a single budget.
pub const DRIVER_MANIFEST_MAX_CAPABILITIES: u16 = 64;

/// Length of the Ed25519 public key embedded in a [`DriverManifest`].
///
/// Matches `rustos_crypto::ED25519_PUBLIC_KEY_LEN`; the byte array is
/// re-declared here so `lib/abi` keeps zero transitive dependencies.
pub const DRIVER_SIGNER_PUBKEY_LEN: usize = 32;

/// Length of the Ed25519 signature embedded in a [`DriverManifest`].
///
/// Matches `rustos_crypto::ED25519_SIGNATURE_LEN`.
pub const DRIVER_SIGNATURE_LEN: usize = 64;

/// Unforgeable opaque handle for a live driver instance.
///
/// The host issues exactly one handle per successful `register` call.
/// The inner integer is opaque — callers must not assume any structure.
///
/// # Errors
///
/// This type is infallible to construct, but methods that consume a
/// [`DriverHandle`] return [`DriverError::NotFound`] if the handle was
/// already unloaded.
///
/// # Capabilities
///
/// Possessing a [`DriverHandle`] is itself the kernel-issued proof that
/// the holder passed the load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) check.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct DriverHandle(u64);

impl DriverHandle {
    /// Sentinel value reserved for "no handle".
    ///
    /// Hosts must never issue this value; it is used internally as the
    /// `Option<DriverHandle>` `None` representation when a fixed-size
    /// wire slot is required.
    pub const NONE: Self = Self(0);

    /// Construct a [`DriverHandle`] from its raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw == 0` (the
    /// [`NONE`](Self::NONE) sentinel).
    ///
    /// # Capabilities
    ///
    /// None directly; the handle itself is the capability proof.
    pub const fn from_raw(raw: u64) -> Result<Self, DriverError> {
        if raw == 0 {
            return Err(DriverError::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw integer carried on the wire.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Whether a driver runs in user space (the default) or in the kernel.
///
/// Per `AGENTS.md` §4 / §8 a driver runs in user space unless the
/// hardware forbids it. Declaring [`DriverKind::InKernel`] in a
/// manifest is what causes the host to require
/// [`CapabilityId::DRV_KERNEL`](crate::CapabilityId::DRV_KERNEL) on
/// top of the universal
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DriverKind {
    /// Driver image runs in an isolated user-space process.
    UserSpace = 0,
    /// Driver image is linked into the kernel address space. Requires
    /// `CAP_DRV_KERNEL` at load time.
    InKernel = 1,
}

impl DriverKind {
    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Construct a [`DriverKind`] from its raw byte.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw` does not name a
    /// known kind.
    ///
    /// # Capabilities
    ///
    /// None.
    pub const fn from_u8(raw: u8) -> Result<Self, DriverError> {
        match raw {
            0 => Ok(Self::UserSpace),
            1 => Ok(Self::InKernel),
            _ => Err(DriverError::OutOfRange),
        }
    }
}

/// Stable error code returned across the driver ABI.
///
/// The variants are kept disjoint from [`Errno`] so that a stray
/// [`DriverError`] returned by a misbehaving driver cannot be confused
/// with a kernel [`Errno`] by the host's dispatcher. Convert with
/// [`DriverError::as_errno`] when bridging into syscalls.
#[repr(i32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DriverError {
    /// A supplied buffer is shorter than the structure it must hold.
    BufferTooSmall = 1,
    /// A magic number, version tag, or reserved field is wrong.
    BadMagic = 2,
    /// The driver manifest targets an ABI version this host does not
    /// support.
    AbiVersionUnsupported = 3,
    /// A length, count, or address field exceeds its ABI maximum.
    LengthOutOfRange = 4,
    /// A discriminant or identifier is outside the table.
    OutOfRange = 5,
    /// The caller does not hold a capability the dispatch site
    /// requires.
    PermissionDenied = 6,
    /// The requested object (handle, device, mount) does not exist.
    NotFound = 7,
    /// The driver manifest signature failed verification.
    SignatureInvalid = 8,
    /// The driver does not implement the requested operation.
    Unsupported = 9,
    /// The underlying hardware reported an unrecoverable fault.
    DeviceFault = 10,
    /// The driver is busy; the caller may retry after backoff.
    Busy = 11,
    /// The requested operation has no implementation in this build.
    NotImplemented = 12,
    /// A storage backend cannot satisfy a request because it is full.
    ///
    /// Emitted by a filesystem driver when it exhausts its free data
    /// space (no free block or cluster remains) or its inode /
    /// directory-entry budget while servicing an allocating operation.
    /// Distinct from [`DeviceFault`](Self::DeviceFault): the device is
    /// healthy, the volume is simply full. Maps to [`Errno::NoSpace`].
    NoSpace = 13,
}

impl DriverError {
    /// Numeric value carried on the ABI.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Construct a [`DriverError`] from its on-ABI numeric value.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw` does not name a
    /// known variant (failing closed on a forged or future code,
    /// `AGENTS.md` §5.4).
    ///
    /// # Capabilities
    ///
    /// None.
    pub const fn from_i32(raw: i32) -> Result<Self, DriverError> {
        match raw {
            1 => Ok(Self::BufferTooSmall),
            2 => Ok(Self::BadMagic),
            3 => Ok(Self::AbiVersionUnsupported),
            4 => Ok(Self::LengthOutOfRange),
            5 => Ok(Self::OutOfRange),
            6 => Ok(Self::PermissionDenied),
            7 => Ok(Self::NotFound),
            8 => Ok(Self::SignatureInvalid),
            9 => Ok(Self::Unsupported),
            10 => Ok(Self::DeviceFault),
            11 => Ok(Self::Busy),
            12 => Ok(Self::NotImplemented),
            13 => Ok(Self::NoSpace),
            _ => Err(Self::OutOfRange),
        }
    }

    /// Map a [`DriverError`] into a kernel [`Errno`] for syscalls that
    /// surface driver outcomes to user space.
    ///
    /// The mapping is a total function: every variant has a stable
    /// counterpart in `abi-v1`'s [`Errno`] surface.
    #[must_use]
    pub const fn as_errno(self) -> Errno {
        match self {
            Self::BufferTooSmall => Errno::BufferTooSmall,
            Self::BadMagic => Errno::BadMagic,
            Self::AbiVersionUnsupported => Errno::AbiVersionUnsupported,
            Self::LengthOutOfRange => Errno::LengthOutOfRange,
            Self::OutOfRange => Errno::OutOfRange,
            Self::PermissionDenied => Errno::PermissionDenied,
            Self::NotFound => Errno::NotFound,
            Self::SignatureInvalid => Errno::SignatureInvalid,
            Self::NoSpace => Errno::NoSpace,
            Self::Unsupported | Self::DeviceFault | Self::Busy | Self::NotImplemented => {
                Errno::NotImplemented
            }
        }
    }
}

/// Fixed-size prefix of a signed driver manifest.
///
/// Field order is part of the frozen `abi-v1` contract. The manifest
/// body that follows the header is a list of [`CapabilityId`] values
/// the driver requests; both halves are covered by
/// [`DriverManifest::signature`]. The signature byte range itself is
/// excluded from coverage; use [`DriverManifest::signed_range`] when
/// verifying.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DriverManifest {
    /// Must equal [`DRIVER_MANIFEST_MAGIC`].
    pub magic: u32,
    /// ABI version this manifest targets; rejected if it does not
    /// match [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// Whether the driver is loaded into user space or the kernel.
    pub kind: DriverKind,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: u8,
    /// Number of [`CapabilityId`] entries in the body. Capped at
    /// [`DRIVER_MANIFEST_MAX_CAPABILITIES`].
    pub capability_count: u16,
    /// SHA-256 of the kernel syscall table the driver was linked
    /// against. Identical in width to the application
    /// [`ManifestHeader`](crate::ManifestHeader) field so a single
    /// verifier can serve both surfaces.
    pub syscall_table_hash: [u8; SYSCALL_TABLE_HASH_LEN],
    /// Ed25519 public key of the signer.
    pub signer_pubkey: [u8; DRIVER_SIGNER_PUBKEY_LEN],
    /// Ed25519 signature over the rest of the manifest.
    pub signature: [u8; DRIVER_SIGNATURE_LEN],
}

impl DriverManifest {
    /// Encoded size of a [`DriverManifest`] on the wire.
    pub const WIRE_LEN: usize = 4 // magic
        + 4 // abi_version
        + 1 // kind
        + 1 // reserved0
        + 2 // capability_count
        + SYSCALL_TABLE_HASH_LEN
        + DRIVER_SIGNER_PUBKEY_LEN
        + DRIVER_SIGNATURE_LEN;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.abi_version.to_le_bytes());
        out[8] = self.kind.as_u8();
        out[9] = self.reserved0;
        out[10..12].copy_from_slice(&self.capability_count.to_le_bytes());
        let mut cursor = 12;
        out[cursor..cursor + SYSCALL_TABLE_HASH_LEN].copy_from_slice(&self.syscall_table_hash);
        cursor += SYSCALL_TABLE_HASH_LEN;
        out[cursor..cursor + DRIVER_SIGNER_PUBKEY_LEN].copy_from_slice(&self.signer_pubkey);
        cursor += DRIVER_SIGNER_PUBKEY_LEN;
        out[cursor..cursor + DRIVER_SIGNATURE_LEN].copy_from_slice(&self.signature);
        out
    }

    /// Decode `bytes` into a [`DriverManifest`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`DriverError::BadMagic`] if the magic word does not match
    ///   or if `reserved0` is non-zero.
    /// * [`DriverError::AbiVersionUnsupported`] if `abi_version` is
    ///   not [`crate::ABI_VERSION_CURRENT`].
    /// * [`DriverError::LengthOutOfRange`] if `capability_count`
    ///   exceeds [`DRIVER_MANIFEST_MAX_CAPABILITIES`].
    /// * [`DriverError::OutOfRange`] if the `kind` byte does not
    ///   name a known [`DriverKind`].
    ///
    /// # Capabilities
    ///
    /// None. Parsing the manifest is a pure operation; the
    /// capability check happens at *load* time against the parsed
    /// body.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != DRIVER_MANIFEST_MAGIC {
            return Err(DriverError::BadMagic);
        }
        let abi_version = read_u32(bytes, 4);
        if abi_version != crate::ABI_VERSION_CURRENT {
            return Err(DriverError::AbiVersionUnsupported);
        }
        let kind = DriverKind::from_u8(bytes[8])?;
        let reserved0 = bytes[9];
        if reserved0 != 0 {
            return Err(DriverError::BadMagic);
        }
        let capability_count = read_u16(bytes, 10);
        if capability_count > DRIVER_MANIFEST_MAX_CAPABILITIES {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut cursor = 12;
        let mut syscall_table_hash = [0u8; SYSCALL_TABLE_HASH_LEN];
        syscall_table_hash.copy_from_slice(&bytes[cursor..cursor + SYSCALL_TABLE_HASH_LEN]);
        cursor += SYSCALL_TABLE_HASH_LEN;
        let mut signer_pubkey = [0u8; DRIVER_SIGNER_PUBKEY_LEN];
        signer_pubkey.copy_from_slice(&bytes[cursor..cursor + DRIVER_SIGNER_PUBKEY_LEN]);
        cursor += DRIVER_SIGNER_PUBKEY_LEN;
        let mut signature = [0u8; DRIVER_SIGNATURE_LEN];
        signature.copy_from_slice(&bytes[cursor..cursor + DRIVER_SIGNATURE_LEN]);
        Ok(Self {
            magic,
            abi_version,
            kind,
            reserved0,
            capability_count,
            syscall_table_hash,
            signer_pubkey,
            signature,
        })
    }

    /// Byte range covered by [`Self::signature`].
    ///
    /// The signature itself sits at the tail of the encoded manifest;
    /// the signer signs every preceding byte.
    #[must_use]
    pub const fn signed_range() -> core::ops::Range<usize> {
        0..(Self::WIRE_LEN - DRIVER_SIGNATURE_LEN)
    }
}

/// Host-supplied environment passed to every driver's `register`
/// entry point.
///
/// Per `AGENTS.md` §8 the host (kernel or user-space driver host) owns
/// the capability set, the audit channel, and the dispatch table. The
/// driver consumes the host only through this trait so it cannot
/// widen its own authority. Concrete implementations of this trait
/// live in the userland driver host (delivered separately per the
/// Stage 4 task split).
///
/// # Capabilities
///
/// Calling any method on a [`DriverHost`] does not itself require a
/// capability; the trait's whole purpose is to *report* the
/// driver-load-time grants the host already enforced.
pub trait DriverHost {
    /// Returns `true` iff the driver was granted `cap` at load time.
    ///
    /// Drivers must call this before invoking any host service that
    /// is gated by `cap`. The host re-checks at the dispatch site
    /// regardless, but consulting the bitmap up front lets a driver
    /// fail fast instead of round-tripping through the host.
    ///
    /// # Errors
    ///
    /// This is a pure query and never fails.
    ///
    /// # Capabilities
    ///
    /// None.
    fn has_capability(&self, cap: CapabilityId) -> bool;

    /// Driver-kind the host loaded this driver as.
    ///
    /// A driver may consult its own kind to choose between
    /// user-space-only and in-kernel-only code paths (for example,
    /// MMIO access patterns).
    ///
    /// # Errors
    ///
    /// Never fails.
    ///
    /// # Capabilities
    ///
    /// None.
    fn kind(&self) -> DriverKind;

    /// Returns the per-driver virtio host, if the driver host has
    /// minted one for this driver module.
    ///
    /// Drivers that consume virtio transports (`virtio_blk`,
    /// `virtio_net`, future virtio-class drivers) call this once at
    /// `register()` time to obtain a [`VirtioHost`] handle the
    /// driver retains (typically inside its driver struct) for the
    /// lifetime of the load. The default implementation returns
    /// `None`, which is the correct shape for every host that does
    /// not (yet) ship virtio-class plumbing — for example a
    /// unit-test seam that drives a non-virtio driver, or the
    /// kernel host before the `kernel-host` feature is enabled on
    /// `rustos-drv-bus-virtio`.
    ///
    /// The signature takes `&self` (not `&mut self`) so it composes
    /// with the frozen driver-load entry point
    /// `pub fn register(host: &dyn DriverHost) -> Result<DriverHandle,
    /// DriverError>` (`AGENTS.md` §8). The returned [`VirtioHost`]
    /// uses interior mutability for its own per-allocation
    /// bookkeeping (see [`VirtioHost`]'s `&self`-based method
    /// signatures), so passing it through an immutable reference is
    /// sound.
    ///
    /// This method is an `abi-v1` *internal* addition (i.e., it
    /// extends the host trait observed by in-tree drivers; the
    /// public driver entry point above is unchanged). The default
    /// body keeps every existing host impl source-compatible.
    ///
    /// # Errors
    ///
    /// Never fails; absence of a virtio host is reported as `None`.
    ///
    /// # Capabilities
    ///
    /// None at the call site; the underlying host enforces capability
    /// checks at each [`VirtioHost`] method.
    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        None
    }

    /// Returns the per-driver MMIO-map facility, if the driver host
    /// has minted one for this driver module.
    ///
    /// Bus drivers (`drivers/bus/pci`, `drivers/bus/mmio`) call this
    /// once they have discovered a device's register block and need
    /// a [`RegisterWindow`] over it. The returned [`MmioMapper`]
    /// enforces [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    /// at each [`map_window`](MmioMapper::map_window) call; the bus
    /// driver never synthesises a pointer itself (`AGENTS.md` §4).
    ///
    /// The default implementation returns `None`, which is the
    /// correct shape for every host that does not (yet) ship the
    /// MMIO-map facility — for example a unit-test seam that drives a
    /// non-bus driver. This is an `abi-v1` *internal* addition that
    /// extends the host trait observed by in-tree drivers; the public
    /// driver entry point is unchanged and the default body keeps
    /// every existing host impl source-compatible.
    ///
    /// # Errors
    ///
    /// Never fails; absence of a mapper is reported as `None`.
    ///
    /// # Capabilities
    ///
    /// None at the call site; the mapper enforces
    /// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP) at
    /// each call.
    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DriverManifest {
        DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: crate::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            reserved0: 0,
            capability_count: 3,
            syscall_table_hash: [0xAA; SYSCALL_TABLE_HASH_LEN],
            signer_pubkey: [0xBB; DRIVER_SIGNER_PUBKEY_LEN],
            signature: [0xCC; DRIVER_SIGNATURE_LEN],
        }
    }

    #[test]
    fn driver_error_discriminants_are_frozen() {
        assert_eq!(DriverError::BufferTooSmall.as_i32(), 1);
        assert_eq!(DriverError::BadMagic.as_i32(), 2);
        assert_eq!(DriverError::AbiVersionUnsupported.as_i32(), 3);
        assert_eq!(DriverError::LengthOutOfRange.as_i32(), 4);
        assert_eq!(DriverError::OutOfRange.as_i32(), 5);
        assert_eq!(DriverError::PermissionDenied.as_i32(), 6);
        assert_eq!(DriverError::NotFound.as_i32(), 7);
        assert_eq!(DriverError::SignatureInvalid.as_i32(), 8);
        assert_eq!(DriverError::Unsupported.as_i32(), 9);
        assert_eq!(DriverError::DeviceFault.as_i32(), 10);
        assert_eq!(DriverError::Busy.as_i32(), 11);
        assert_eq!(DriverError::NotImplemented.as_i32(), 12);
        assert_eq!(DriverError::NoSpace.as_i32(), 13);
    }

    #[test]
    fn driver_error_maps_to_errno() {
        assert_eq!(
            DriverError::PermissionDenied.as_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(DriverError::NotFound.as_errno(), Errno::NotFound);
        assert_eq!(DriverError::Busy.as_errno(), Errno::NotImplemented);
        assert_eq!(DriverError::NoSpace.as_errno(), Errno::NoSpace);
    }

    #[test]
    fn driver_error_from_i32_round_trips_and_fails_closed() {
        let all = [
            DriverError::BufferTooSmall,
            DriverError::BadMagic,
            DriverError::AbiVersionUnsupported,
            DriverError::LengthOutOfRange,
            DriverError::OutOfRange,
            DriverError::PermissionDenied,
            DriverError::NotFound,
            DriverError::SignatureInvalid,
            DriverError::Unsupported,
            DriverError::DeviceFault,
            DriverError::Busy,
            DriverError::NotImplemented,
            DriverError::NoSpace,
        ];
        for err in all {
            assert_eq!(DriverError::from_i32(err.as_i32()), Ok(err));
        }
        assert_eq!(DriverError::from_i32(0), Err(DriverError::OutOfRange));
        assert_eq!(DriverError::from_i32(14), Err(DriverError::OutOfRange));
        assert_eq!(DriverError::from_i32(-1), Err(DriverError::OutOfRange));
    }

    #[test]
    fn driver_kind_round_trip() {
        assert_eq!(DriverKind::from_u8(0), Ok(DriverKind::UserSpace));
        assert_eq!(DriverKind::from_u8(1), Ok(DriverKind::InKernel));
        assert_eq!(DriverKind::from_u8(2), Err(DriverError::OutOfRange));
        assert_eq!(DriverKind::UserSpace.as_u8(), 0);
        assert_eq!(DriverKind::InKernel.as_u8(), 1);
    }

    #[test]
    fn driver_handle_rejects_sentinel() {
        assert_eq!(DriverHandle::from_raw(0), Err(DriverError::OutOfRange));
        let Ok(h) = DriverHandle::from_raw(42) else {
            unreachable!("42 is non-zero")
        };
        assert_eq!(h.as_u64(), 42);
        assert_ne!(h, DriverHandle::NONE);
    }

    #[test]
    fn manifest_wire_size_matches_struct() {
        // Manifest fields must add up to the byte budget.
        assert_eq!(
            DriverManifest::WIRE_LEN,
            4 + 4 + 1 + 1 + 2 + SYSCALL_TABLE_HASH_LEN + 32 + 64
        );
    }

    #[test]
    fn manifest_round_trip() {
        let m = sample();
        let bytes = m.to_le_bytes();
        let Ok(decoded) = DriverManifest::from_bytes(&bytes) else {
            unreachable!("encoded sample must decode")
        };
        assert_eq!(decoded, m);
    }

    #[test]
    fn manifest_rejects_short_buffer() {
        let buf = [0u8; 8];
        assert_eq!(
            DriverManifest::from_bytes(&buf),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn manifest_rejects_bad_magic() {
        let mut bytes = sample().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::BadMagic)
        );
    }

    #[test]
    fn manifest_rejects_bad_abi_version() {
        let mut m = sample();
        m.abi_version = crate::ABI_VERSION_CURRENT + 1;
        let bytes = m.to_le_bytes();
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::AbiVersionUnsupported),
        );
    }

    #[test]
    fn manifest_rejects_excess_capabilities() {
        let mut m = sample();
        m.capability_count = DRIVER_MANIFEST_MAX_CAPABILITIES + 1;
        let bytes = m.to_le_bytes();
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::LengthOutOfRange),
        );
    }

    #[test]
    fn manifest_rejects_nonzero_reserved() {
        let mut bytes = sample().to_le_bytes();
        bytes[9] = 1;
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::BadMagic)
        );
    }

    #[test]
    fn manifest_rejects_unknown_kind() {
        let mut bytes = sample().to_le_bytes();
        bytes[8] = 0x7F;
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn signed_range_excludes_signature_tail() {
        let r = DriverManifest::signed_range();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, DriverManifest::WIRE_LEN - DRIVER_SIGNATURE_LEN);
    }

    struct StubHost;

    impl DriverHost for StubHost {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD
        }

        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
    }

    #[test]
    fn buffer_class_round_trip() {
        assert_eq!(BufferClass::NonSensitive.as_u8(), 0);
        assert_eq!(BufferClass::Sensitive.as_u8(), 1);
        assert_eq!(BufferClass::from_u8(0), Ok(BufferClass::NonSensitive));
        assert_eq!(BufferClass::from_u8(1), Ok(BufferClass::Sensitive));
        assert_eq!(BufferClass::from_u8(2), Err(DriverError::OutOfRange));
        assert!(BufferClass::Sensitive.is_sensitive());
        assert!(!BufferClass::NonSensitive.is_sensitive());
    }

    #[test]
    fn driver_host_capability_gate_metadata() {
        let host = StubHost;
        assert!(host.has_capability(CapabilityId::DRV_LOAD));
        assert!(!host.has_capability(CapabilityId::DRV_KERNEL));
        assert_eq!(host.kind(), DriverKind::UserSpace);
    }
}
