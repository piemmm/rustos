//! Driver class trait surface (Stage 4 of `PLAN.md`).
//!
//! This module is the single source of truth for the user/kernel ABI that
//! sits between a loaded driver module (`.rxe` binary)
//! and its host. The host lives in user space by default
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
//!    site*; the trait implementation never re-checks.
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

use crate::hwtree::{HwMatchKey, HwNode, HwResource, MsiAllocation};
use crate::le::{put_u16, read_u16, read_u32};
use crate::syscall::SYSCALL_TABLE_HASH_LEN;
use crate::{CapabilityId, Errno};

pub mod block;
pub mod bus;
pub mod display;
pub mod dma;
pub mod filesystem;
pub mod input;
pub mod mailbox;
pub mod mmio;
pub mod msix;
pub mod net;
pub mod pci;
pub mod port_io;
pub mod register;
pub mod timing;
pub mod virtio;
pub mod virtio_mmio;
pub mod virtio_pci;

pub use dma::{DmaHost, DmaSlab, PoolId, SlabFreeFn};
pub use mailbox::MailboxChannel;
pub use mmio::{MmioMapError, MmioMapper, RegisterWindow, WindowError};
pub use msix::{MsiMessage, MsixBus};
pub use pci::PciBus;
pub use port_io::{PortIo, PortIo8};
pub use register::{DriverRegisterReply, DRIVER_REGISTER_REPLY_MAGIC, DRIVER_REGISTER_STATUS_OK};
pub use timing::Delay;
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
/// soon as a payload leaves them ("zero-on-free for
/// any allocation that ever held credentials, keys, or capability
/// tokens"). The flag is a *promise about the buffer's contents*,
/// not an access-control gate: capability enforcement remains at the
/// dispatch site.
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
/// closed).
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
    /// known class. The host must fail closed in that case.
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

/// Maximum number of [`DriverBindKey`] entries a single driver manifest
/// may declare.
///
/// Bounded so that a hostile manifest cannot force unbounded parsing
/// work (a validation bound, not a capacity). A
/// driver binds one device class on a handful of buses; sixteen keys is
/// generous headroom for every in-tree driver.
pub const DRIVER_MANIFEST_MAX_BIND_KEYS: u8 = 16;

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
/// Per a driver runs in user space unless the
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
    /// The presenting client's seat lease was forcibly revoked.
    ///
    /// Returned by a display driver's present path when the host's
    /// [`SeatGate`](display::SeatGate) reports the client lost its seat to
    /// an administrative `seat_revoke`. Distinct from
    /// [`PermissionDenied`](Self::PermissionDenied) so a well-behaved
    /// compositor learns it lost the seat rather than treating the refusal
    /// as a generic authority failure. Maps to [`Errno::SeatRevoked`].
    SeatRevoked = 14,
    /// A device endpoint answered the transfer with a protocol STALL.
    ///
    /// Returned by the USB URB transport's bulk path when the device halts
    /// the addressed endpoint — an in-band protocol signal (USB BOT rejects
    /// a phase with a bulk STALL), not the unrecoverable hardware error
    /// [`DeviceFault`](Self::DeviceFault) reports. The host-controller
    /// driver has already recovered the endpoint (halt cleared, ring
    /// repositioned) when this is delivered, so the caller may run its own
    /// class-level recovery and submit fresh transfers immediately. Maps to
    /// [`Errno::EndpointStalled`].
    EndpointStalled = 15,
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
    /// known variant (failing closed on a forged or future code).
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
            14 => Ok(Self::SeatRevoked),
            15 => Ok(Self::EndpointStalled),
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
            Self::SeatRevoked => Errno::SeatRevoked,
            Self::EndpointStalled => Errno::EndpointStalled,
            // A faulted device is its own client-visible condition; a busy
            // one is retryable (`WouldBlock`); an unsupported operation
            // reads as not implemented.
            Self::DeviceFault => Errno::DeviceFault,
            Self::Busy => Errno::WouldBlock,
            Self::Unsupported | Self::NotImplemented => Errno::NotImplemented,
        }
    }
}

/// Fixed-size prefix of a signed driver manifest.
///
/// Field order is part of the frozen `abi-v1` contract. The body that
/// follows the header is a list of [`CapabilityId`] values the driver
/// requests (`capability_count` entries) followed by the driver's bind
/// table (`bind_key_count` [`DriverBindKey`] records); header and body are covered by
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
    /// Number of [`DriverBindKey`] entries in the body, following the
    /// capability list. Capped at [`DRIVER_MANIFEST_MAX_BIND_KEYS`].
    pub bind_key_count: u8,
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
        + 1 // bind_key_count
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
        out[9] = self.bind_key_count;
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
    /// * [`DriverError::BadMagic`] if the magic word does not match.
    /// * [`DriverError::AbiVersionUnsupported`] if `abi_version` is
    ///   not [`crate::ABI_VERSION_CURRENT`].
    /// * [`DriverError::LengthOutOfRange`] if `capability_count`
    ///   exceeds [`DRIVER_MANIFEST_MAX_CAPABILITIES`] or
    ///   `bind_key_count` exceeds [`DRIVER_MANIFEST_MAX_BIND_KEYS`].
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
        let bind_key_count = bytes[9];
        if bind_key_count > DRIVER_MANIFEST_MAX_BIND_KEYS {
            return Err(DriverError::LengthOutOfRange);
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
            bind_key_count,
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

/// One entry of a driver manifest's bind table.
///
/// The bind table is how a driver declares the hardware-tree nodes it
/// can drive: each entry pairs one [`HwMatchKey`]
/// with a manifest-declared bind priority. The device manager compares
/// a node's match keys against every loaded manifest's table; when more
/// than one driver matches the same node, the higher matched `priority`
/// binds. An unbroken tie is a packaging defect the device manager
/// refuses deterministically — never a coin-flip.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DriverBindKey {
    /// Bind priority; a higher value binds in preference to a lower
    /// one when two drivers match the same node.
    pub priority: u16,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: u16,
    /// The hardware-tree match key this entry binds to.
    pub key: HwMatchKey,
}

impl DriverBindKey {
    /// Encoded size of a [`DriverBindKey`] on the wire.
    pub const WIRE_LEN: usize = 4 + HwMatchKey::WIRE_LEN;

    /// A bind-table entry binding `key` at `priority`.
    #[must_use]
    pub const fn new(priority: u16, key: HwMatchKey) -> Self {
        Self {
            priority,
            reserved0: 0,
            key,
        }
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u16(&mut out, 0, self.priority);
        put_u16(&mut out, 2, self.reserved0);
        out[4..].copy_from_slice(&self.key.to_le_bytes());
        out
    }

    /// Decode `bytes` into a [`DriverBindKey`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`DriverError::BadMagic`] if `reserved0` is non-zero.
    /// * [`DriverError::OutOfRange`] if the embedded key's kind is
    ///   unknown.
    /// * [`DriverError::LengthOutOfRange`] if the embedded key's
    ///   `compatible` length exceeds its bound.
    ///
    /// # Capabilities
    ///
    /// None. Parsing is pure; the device manager's load gate enforces
    /// the capability checks.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        let priority = read_u16(bytes, 0);
        let reserved0 = read_u16(bytes, 2);
        if reserved0 != 0 {
            return Err(DriverError::BadMagic);
        }
        let key = HwMatchKey::from_bytes(&bytes[4..Self::WIRE_LEN]).map_err(|e| match e {
            Errno::BufferTooSmall => DriverError::BufferTooSmall,
            Errno::LengthOutOfRange => DriverError::LengthOutOfRange,
            _ => DriverError::OutOfRange,
        })?;
        Ok(Self {
            priority,
            reserved0,
            key,
        })
    }
}

/// Decode the bind table that follows a driver manifest's capability
/// body.
///
/// The table is `count` consecutive [`DriverBindKey`] records, where
/// `count` is the manifest's `bind_key_count` field. Decoded entries
/// are written into `out`; the number written (always `count` on
/// success) is returned so a fixed-size scratch buffer can be reused
/// across manifests. This is the single decoder for the table format,
/// shared by every consumer that turns a signed manifest into a bind
/// table.
///
/// # Errors
///
/// * [`DriverError::BufferTooSmall`] if `out` cannot hold `count`
///   entries, or if `body` is shorter than
///   `count * DriverBindKey::WIRE_LEN` bytes.
/// * Any [`DriverBindKey::from_bytes`] error for an invalid entry.
///
/// # Capabilities
///
/// None. Parsing is pure; the load gate enforces capability checks.
pub fn decode_bind_keys(
    body: &[u8],
    count: usize,
    out: &mut [DriverBindKey],
) -> Result<usize, DriverError> {
    if out.len() < count {
        return Err(DriverError::BufferTooSmall);
    }
    let needed = count
        .checked_mul(DriverBindKey::WIRE_LEN)
        .ok_or(DriverError::LengthOutOfRange)?;
    if body.len() < needed {
        return Err(DriverError::BufferTooSmall);
    }
    for (i, slot) in out.iter_mut().enumerate().take(count) {
        *slot = DriverBindKey::from_bytes(&body[i * DriverBindKey::WIRE_LEN..])?;
    }
    Ok(count)
}

/// Resolve the *single* mappable register window from a driver's
/// kernel-issued device-resource grants.
///
/// A `devmgr`-autoloaded driver is granted exactly the resources its
/// matched hardware-tree node requested — and no more. A bus/device driver maps one register block by *address*
/// through its [`DriverHost`]'s [`MmioMapper`] (which resolves the
/// covering grant and performs any bus→CPU translation); this finds that one window and returns the
/// `(base, len)` pair to map. The base is whichever address names the
/// window for its kind — [`HwResource::register_window_base`] is the one
/// definition of that choice, so the same derivation
/// serves every device class (the USB keyboard's BAR, a virtio MMIO
/// transport's register block) without re-deciding `base` vs
/// `translated_base`.
///
/// Non-window resources (a DMA constraint, an IRQ line, a port range)
/// are ignored — they are carved or waited on through other syscalls,
/// not mapped here.
///
/// # Errors
///
/// Fails closed, never guessing a missing or
/// ambiguous address:
///
/// * [`DriverError::NotFound`] if no register-window grant is present.
/// * [`DriverError::Unsupported`] if more than one register-window grant
///   is present (an ambiguous delivery — a packaging defect the driver
///   refuses rather than picking one).
/// * [`DriverError::OutOfRange`] for a zero-length window or a length
///   past `usize` on the target.
///
/// # Capabilities
///
/// None. This inspects a grant set the kernel already minted; the map
/// itself is capability-checked kernel-side at the `mmio_map` trap.
pub fn sole_register_window<'a, I>(resources: I) -> Result<(u64, usize), DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut window: Option<(u64, u64)> = None;
    for resource in resources {
        if let Some(base) = resource.register_window_base() {
            if window.is_some() {
                return Err(DriverError::Unsupported);
            }
            window = Some((base, resource.length()));
        }
    }
    let (base, len) = window.ok_or(DriverError::NotFound)?;
    let len = usize::try_from(len).map_err(|_| DriverError::OutOfRange)?;
    if len == 0 {
        return Err(DriverError::OutOfRange);
    }
    Ok((base, len))
}

/// Resolve the *single* linear scan-out surface from a driver's
/// kernel-issued device-resource grants: the
/// [`Framebuffer`](crate::hwtree::HwResourceKind::Framebuffer) resource a
/// display-class node carries, returned as the window's CPU-physical
/// base and its validated
/// [`DisplayMode`](crate::driver::display::DisplayMode)
/// (`plans/DISPLAY.md` D7b).
///
/// The sibling of [`sole_register_window`] for the display class: the
/// autoloaded display service builds its surface from exactly this grant
/// and nothing else, so a mis-provisioned node fails the bring-up rather
/// than scanning out a guessed geometry.
///
/// # Errors
///
/// Fails closed, never guessing a missing or ambiguous surface:
///
/// * [`DriverError::NotFound`] if no framebuffer grant is present.
/// * [`DriverError::Unsupported`] if more than one is present (an
///   ambiguous delivery — a packaging defect the driver refuses rather
///   than picking one).
/// * [`DriverError::BadMagic`] / [`DriverError::OutOfRange`] /
///   [`DriverError::LengthOutOfRange`] if the resource's geometry does
///   not validate ([`HwResource::framebuffer_mode`]).
///
/// # Capabilities
///
/// None. This inspects a grant set the kernel already minted; the map
/// itself is capability-checked kernel-side at the `mmio_map` trap.
pub fn sole_framebuffer<'a, I>(
    resources: I,
) -> Result<(u64, crate::driver::display::DisplayMode), DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut surface: Option<&HwResource> = None;
    for resource in resources {
        if resource.kind() == Some(crate::hwtree::HwResourceKind::Framebuffer) {
            if surface.is_some() {
                return Err(DriverError::Unsupported);
            }
            surface = Some(resource);
        }
    }
    let resource = surface.ok_or(DriverError::NotFound)?;
    let mode = resource.framebuffer_mode().map_err(|err| match err {
        Errno::BadMagic => DriverError::BadMagic,
        Errno::LengthOutOfRange => DriverError::LengthOutOfRange,
        _ => DriverError::OutOfRange,
    })?;
    Ok((resource.base(), mode))
}

/// Host-supplied environment passed to every driver's `register`
/// entry point.
///
/// Per the host (kernel or user-space driver host) owns
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
    /// DriverError>`. The returned [`VirtioHost`]
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
    /// Bus drivers (`drivers/bus/pcie_brcm`, `drivers/bus/mmio`) call this
    /// once they have discovered a device's register block and need
    /// a [`RegisterWindow`] over it. The returned [`MmioMapper`]
    /// enforces [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    /// at each [`map_window`](MmioMapper::map_window) call; the bus
    /// driver never synthesises a pointer itself.
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

    /// Returns the per-driver DMA-allocation facility, if the driver host
    /// has minted one for this driver module.
    ///
    /// This is the bus-neutral sibling of [`mmio_mapper`](Self::mmio_mapper):
    /// a bus driver that has to hand the hardware a physically-addressable
    /// buffer (an xHCI device-context array, event/command/transfer rings, a
    /// scratchpad) obtains a [`DmaSlab`] through the returned [`DmaHost`]. A
    /// virtio driver still uses [`virtio_host`](Self::virtio_host), which
    /// extends [`DmaHost`]; this accessor exists so a *non*-virtio driver
    /// never has to reach through a virtio-shaped trait to allocate DMA
    /// (the allocation contract is defined once, in
    /// [`DmaHost`]).
    ///
    /// The default implementation returns `None`, the correct shape for a
    /// host that ships no DMA facility (a unit-test seam for a driver that
    /// needs none). This is an `abi-v1` *internal* addition that extends the
    /// host trait observed by in-tree drivers; the public driver entry point
    /// is unchanged and the default body keeps every existing host impl
    /// source-compatible.
    ///
    /// # Errors
    ///
    /// Never fails; absence of a DMA host is reported as `None`.
    ///
    /// # Capabilities
    ///
    /// None at the call site; the [`DmaHost`] enforces the host's per-task
    /// DMA capability check at each allocation.
    fn dma_host(&self) -> Option<&dyn DmaHost> {
        None
    }

    /// Returns the per-driver firmware property-mailbox channel, if the
    /// driver host has wired one for this driver module.
    ///
    /// A bus driver whose bring-up needs the platform firmware — for example
    /// the BCM2711 `VideoCore` reload of the VL805 USB controller's firmware —
    /// obtains a board-neutral [`MailboxChannel`] here and marshals encoded
    /// property messages through it. The doorbell window, the DMA-backed
    /// property buffer, and the bus-address translation are owned by the
    /// host; the board specifics stay behind the device's own crate
    /// (`lib/vcmailbox`), so the generic framework above it
    /// never names a board.
    ///
    /// The default implementation returns `None`, the correct shape for every
    /// platform with no firmware mailbox (QEMU `virt`, x86_64, riscv64) and
    /// for any test seam that drives a driver needing none (a missing facility is silent, never an error).
    ///
    /// # Errors
    ///
    /// Never fails; absence of a mailbox is reported as `None`.
    ///
    /// # Capabilities
    ///
    /// None at the call site; the channel's host enforces the capability gate
    /// for the doorbell MMIO and property-buffer DMA it owns.
    fn mailbox(&self) -> Option<&dyn MailboxChannel> {
        None
    }

    /// Publish a discovered child [`HwNode`] into the hardware tree.
    ///
    /// A bus driver that enumerates a device behind it (a PCIe function, a USB
    /// device on a root-hub port) calls this to attach the device as a child
    /// node carrying the [`HwResource`] grant *requests* the matched
    /// downstream driver will receive — for example a USB-HID node carrying
    /// its xHCI register-window and DMA-region resources. The host validates
    /// the node, mints it into the live tree, and the match/autoload
    /// path then sees a bindable node like any other discovered device. The
    /// driver requests only the resources its enumeration actually found; the
    /// host grants nothing the node did not request (no
    /// ambient authority).
    ///
    /// The default implementation returns [`DriverError::Unsupported`], the
    /// correct shape for a host with no hardware-tree producer wired (a
    /// unit-test seam, or a host that loads only leaf drivers that never
    /// enumerate children). This is an `abi-v1` *internal* addition; the
    /// default body keeps every existing host impl source-compatible.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the host exposes no hardware-tree
    ///   producer (the default).
    /// * [`DriverError::PermissionDenied`] if the host's capability check for
    ///   emitting a node fails.
    /// * [`DriverError::OutOfRange`] / [`DriverError::NoSpace`] if the node is
    ///   malformed or the tree is full (the host fails closed).
    ///
    /// # Capabilities
    ///
    /// None at the call site; the host enforces its own gate on tree
    /// mutation.
    fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
        let _ = node;
        Err(DriverError::Unsupported)
    }

    /// Allocate a message-signalled interrupt (MSI) vector for a PCI
    /// function this bus driver enumerated, returning the [`MsiAllocation`]
    /// the kernel minted — the virtual interrupt line plus the doorbell
    /// `(address, data)` to program into the function's MSI capability.
    ///
    /// A PCI bus driver wiring a function for MSI (the BCM2711 PCIe driver
    /// arming the VL805 xHCI) calls this, programs the function's MSI
    /// capability with the returned doorbell (`PciBus`-side), and forwards
    /// the returned line as an [`HwResource::irq`] on the child node it
    /// publishes through [`Self::emit_node`], so the downstream driver binds
    /// it with `irq_bind`/`irq_wait`. The kernel grants the calling task a
    /// device resource for the line, so the forwarded resource is covered by
    /// a grant the emitter already holds (no ambient authority).
    ///
    /// The default implementation returns [`DriverError::Unsupported`], the
    /// correct shape for a host with no MSI facility wired (a unit-test seam,
    /// or a platform with no MSI controller). This is an `abi-v1` *internal*
    /// addition; the default body keeps every existing host impl
    /// source-compatible.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the host exposes no MSI facility
    ///   (the default).
    /// * [`DriverError::PermissionDenied`] if the host's `CAP_IRQ_BIND`
    ///   check fails.
    /// * [`DriverError::OutOfRange`] if the platform's MSI vector space is
    ///   exhausted, or [`DriverError::NotImplemented`] on a platform with no
    ///   MSI controller (the host fails closed).
    ///
    /// # Capabilities
    ///
    /// None at the call site; the kernel enforces `CAP_IRQ_BIND` on the
    /// underlying `msi_alloc` syscall.
    fn alloc_msi(&self) -> Result<MsiAllocation, DriverError> {
        Err(DriverError::Unsupported)
    }

    /// Returns the live seat-lease gate for the client this host presents
    /// on behalf of, if the host has wired a seat.
    ///
    /// A display driver consults the returned
    /// [`SeatGate`](display::SeatGate) at the top of every present/flip,
    /// so the present right is derived from the client's *current* seat
    /// lease rather than from its (still-mapped) framebuffer window: a
    /// revoked client cannot scan out over the new foreground. The gate is
    /// bound to the client's [`SeatLease`](crate::seat::SeatLease) by the
    /// host — the driver never sees or supplies the handle.
    ///
    /// The default implementation returns `None`, the correct shape for a
    /// host with no seat wired: a headless build, a boot-console
    /// bring-up surface, or a unit-test seam — there is no lease to derive
    /// the right from, so the driver presents ungated for that host. This
    /// is an `abi-v1` *internal* addition; the default body keeps every
    /// existing host impl source-compatible.
    ///
    /// # Errors
    ///
    /// Never fails; absence of a seat is reported as `None`.
    ///
    /// # Capabilities
    ///
    /// None at the call site; the gate itself checks the client's lease
    /// against the kernel seat registry on every call.
    fn seat_gate(&self) -> Option<&dyn display::SeatGate> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwtree::HwDeviceClass;

    fn sample() -> DriverManifest {
        DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: crate::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: 0,
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
        assert_eq!(DriverError::SeatRevoked.as_i32(), 14);
        assert_eq!(DriverError::EndpointStalled.as_i32(), 15);
    }

    #[test]
    fn driver_error_maps_to_errno() {
        assert_eq!(
            DriverError::PermissionDenied.as_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(DriverError::NotFound.as_errno(), Errno::NotFound);
        assert_eq!(DriverError::Busy.as_errno(), Errno::WouldBlock);
        assert_eq!(DriverError::DeviceFault.as_errno(), Errno::DeviceFault);
        assert_eq!(DriverError::Unsupported.as_errno(), Errno::NotImplemented);
        assert_eq!(DriverError::NoSpace.as_errno(), Errno::NoSpace);
        assert_eq!(DriverError::SeatRevoked.as_errno(), Errno::SeatRevoked);
        assert_eq!(
            DriverError::EndpointStalled.as_errno(),
            Errno::EndpointStalled
        );
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
            DriverError::SeatRevoked,
            DriverError::EndpointStalled,
        ];
        for err in all {
            assert_eq!(DriverError::from_i32(err.as_i32()), Ok(err));
        }
        assert_eq!(DriverError::from_i32(0), Err(DriverError::OutOfRange));
        assert_eq!(DriverError::from_i32(16), Err(DriverError::OutOfRange));
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
    fn manifest_rejects_excess_bind_keys() {
        let mut bytes = sample().to_le_bytes();
        bytes[9] = DRIVER_MANIFEST_MAX_BIND_KEYS + 1;
        assert_eq!(
            DriverManifest::from_bytes(&bytes),
            Err(DriverError::LengthOutOfRange)
        );
    }

    #[test]
    fn manifest_round_trips_bind_key_count() {
        let mut m = sample();
        m.bind_key_count = DRIVER_MANIFEST_MAX_BIND_KEYS;
        let bytes = m.to_le_bytes();
        assert_eq!(DriverManifest::from_bytes(&bytes), Ok(m));
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

    #[test]
    fn driver_host_seat_gate_defaults_to_absent() {
        // A host with no seat wired exposes no gate: there is no lease to
        // derive the present right from (a headless or bring-up host).
        let host = StubHost;
        assert!(host.seat_gate().is_none());
    }

    #[test]
    fn driver_host_facility_accessors_default_to_absent() {
        // A host that wires no facilities reports each optional accessor as
        // absent, never as an error or a synthesised handle (a missing facility is silent; the bus driver fails closed).
        let host = StubHost;
        assert!(host.virtio_host().is_none());
        assert!(host.mmio_mapper().is_none());
        assert!(host.dma_host().is_none());
        assert!(host.mailbox().is_none());
        // `emit_node` defaults to refusing, never to silently accepting a
        // node a host with no tree producer cannot publish (fail closed).
        let node = HwNode::new(1, crate::hwtree::HW_NODE_ROOT, HwDeviceClass::Input);
        assert_eq!(host.emit_node(node), Err(DriverError::Unsupported));
    }

    /// Host wiring the DMA, mailbox, and node-emission facilities, used to
    /// prove a floor bus driver reaches each through the [`DriverHost`]
    /// contract alone (the surface the autonomous bring-up consumes).
    struct FacilityHost {
        emitted: core::cell::Cell<u32>,
        last_emitted: core::cell::Cell<Option<HwNode>>,
    }

    impl FacilityHost {
        fn new() -> Self {
            Self {
                emitted: core::cell::Cell::new(0),
                last_emitted: core::cell::Cell::new(None),
            }
        }
    }

    impl DmaHost for FacilityHost {
        fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
            // No-alloc test seam: exercise the documented rejections without
            // minting a slab (`lib/abi` is no-alloc). A real host returns a
            // zeroed [`DmaSlab`]; the seam's contract — reject `size == 0`,
            // surface pool pressure — is what the ABI test pins.
            if size == 0 {
                return Err(DriverError::BufferTooSmall);
            }
            Err(DriverError::LengthOutOfRange)
        }
    }

    impl MailboxChannel for FacilityHost {
        fn exchange(
            &self,
            message: &mut [u32; crate::driver::mailbox::MAILBOX_PROPERTY_WORDS],
        ) -> Result<(), DriverError> {
            // Echo a success response code into the header so a caller can
            // observe the in-place round trip the seam guarantees.
            message[1] = 0x8000_0000;
            Ok(())
        }
    }

    impl DriverHost for FacilityHost {
        fn has_capability(&self, _cap: CapabilityId) -> bool {
            true
        }

        fn kind(&self) -> DriverKind {
            DriverKind::InKernel
        }

        fn dma_host(&self) -> Option<&dyn DmaHost> {
            Some(self)
        }

        fn mailbox(&self) -> Option<&dyn MailboxChannel> {
            Some(self)
        }

        fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
            self.last_emitted.set(Some(node));
            self.emitted.set(self.emitted.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn driver_host_routes_dma_allocation_through_the_seam() {
        let host = FacilityHost::new();
        let dma = host.dma_host().expect("dma host wired");
        // `DmaSlab` is not `PartialEq` (it owns a device pointer), so match
        // on the seam's error contract rather than comparing the `Result`.
        assert!(matches!(
            dma.alloc_dma_zeroed(0),
            Err(DriverError::BufferTooSmall)
        ));
        assert!(matches!(
            dma.alloc_dma_zeroed(4096),
            Err(DriverError::LengthOutOfRange)
        ));
    }

    #[test]
    fn driver_host_routes_mailbox_exchange_through_the_seam() {
        let host = FacilityHost::new();
        let mailbox = host.mailbox().expect("mailbox wired");
        let mut message = [0u32; crate::driver::mailbox::MAILBOX_PROPERTY_WORDS];
        mailbox.exchange(&mut message).expect("exchange");
        // The response was written back in place (the
        // caller reads the firmware response from the same buffer).
        assert_eq!(message[1], 0x8000_0000);
    }

    #[test]
    fn driver_host_publishes_an_emitted_node() {
        let host = FacilityHost::new();
        let node = HwNode::new(42, crate::hwtree::HW_NODE_ROOT_ID, HwDeviceClass::Input);
        assert_eq!(host.emit_node(node), Ok(()));
        assert_eq!(host.emitted.get(), 1);
        assert_eq!(host.last_emitted.get(), Some(node));
    }

    fn sample_bind_key() -> DriverBindKey {
        let Ok(key) = HwMatchKey::compatible(b"brcm,bcm2711-emmc2") else {
            unreachable!("compatible string fits HW_COMPATIBLE_MAX")
        };
        DriverBindKey::new(10, key)
    }

    #[test]
    fn bind_key_wire_size_matches_struct() {
        assert_eq!(
            DriverBindKey::WIRE_LEN,
            core::mem::size_of::<DriverBindKey>()
        );
        assert_eq!(DriverBindKey::WIRE_LEN, 4 + HwMatchKey::WIRE_LEN);
    }

    #[test]
    fn bind_key_round_trips() {
        let entry = sample_bind_key();
        let bytes = entry.to_le_bytes();
        assert_eq!(DriverBindKey::from_bytes(&bytes), Ok(entry));
        let numeric = DriverBindKey::new(0, HwMatchKey::pci(0x1AF4, 0x1042, 0x0001_0000));
        let bytes = numeric.to_le_bytes();
        assert_eq!(DriverBindKey::from_bytes(&bytes), Ok(numeric));
    }

    #[test]
    fn bind_key_rejects_short_reserved_and_bad_kind() {
        let entry = sample_bind_key();
        let bytes = entry.to_le_bytes();
        assert_eq!(
            DriverBindKey::from_bytes(&bytes[..DriverBindKey::WIRE_LEN - 1]),
            Err(DriverError::BufferTooSmall)
        );
        let mut nonzero_reserved = bytes;
        nonzero_reserved[2] = 1;
        assert_eq!(
            DriverBindKey::from_bytes(&nonzero_reserved),
            Err(DriverError::BadMagic)
        );
        let mut bad_kind = bytes;
        bad_kind[4] = 0xFF;
        assert_eq!(
            DriverBindKey::from_bytes(&bad_kind),
            Err(DriverError::OutOfRange)
        );
        let mut overlong = bytes;
        overlong[6] = 0xFF; // compatible_len beyond HW_COMPATIBLE_MAX
        assert_eq!(
            DriverBindKey::from_bytes(&overlong),
            Err(DriverError::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_bind_keys_round_trips() {
        let entries = [
            sample_bind_key(),
            DriverBindKey::new(2, HwMatchKey::virtio(2)),
        ];
        let mut body = [0u8; 2 * DriverBindKey::WIRE_LEN];
        for (i, e) in entries.iter().enumerate() {
            body[i * DriverBindKey::WIRE_LEN..(i + 1) * DriverBindKey::WIRE_LEN]
                .copy_from_slice(&e.to_le_bytes());
        }
        let mut out = [DriverBindKey::new(0, HwMatchKey::virtio(0)); 4];
        assert_eq!(decode_bind_keys(&body, 2, &mut out), Ok(2));
        assert_eq!(&out[..2], &entries);
    }

    #[test]
    fn decode_bind_keys_fails_closed() {
        let entry = sample_bind_key();
        let body = entry.to_le_bytes();
        let mut small_out: [DriverBindKey; 0] = [];
        assert_eq!(
            decode_bind_keys(&body, 1, &mut small_out),
            Err(DriverError::BufferTooSmall)
        );
        let mut out = [DriverBindKey::new(0, HwMatchKey::virtio(0)); 2];
        assert_eq!(
            decode_bind_keys(&body, 2, &mut out),
            Err(DriverError::BufferTooSmall)
        );
        let mut bad = body;
        bad[2] = 1;
        assert_eq!(
            decode_bind_keys(&bad, 1, &mut out),
            Err(DriverError::BadMagic)
        );
    }

    #[test]
    fn sole_register_window_resolves_an_mmio_window_by_its_cpu_base() {
        let grants = [
            HwResource::dma(0x8000_0000, 0),
            HwResource::mmio(0x1000_0000, 0x1000),
            HwResource::irq(33, 1),
        ];
        assert_eq!(
            sole_register_window(grants.iter()),
            Ok((0x1000_0000, 0x1000))
        );
    }

    #[test]
    fn sole_register_window_resolves_a_bus_window_by_its_translated_base() {
        // A `BusWindow` is named by its far-side (bus-space) base, not its
        // CPU base.
        let grants = [HwResource::bus_window(0x6_0000_0000, 0x2000, 0x4000_0000)];
        assert_eq!(
            sole_register_window(grants.iter()),
            Ok((0x4000_0000, 0x2000))
        );
    }

    #[test]
    fn sole_register_window_fails_closed() {
        // No window grant.
        assert_eq!(
            sole_register_window([HwResource::dma(0x8000_0000, 0)].iter()),
            Err(DriverError::NotFound)
        );
        // Two window grants — ambiguous, refused rather than guessed.
        let two = [
            HwResource::mmio(0x1000_0000, 0x1000),
            HwResource::bus_window(0x6_0000_0000, 0x2000, 0x4000_0000),
        ];
        assert_eq!(
            sole_register_window(two.iter()),
            Err(DriverError::Unsupported)
        );
        // Zero-length window.
        assert_eq!(
            sole_register_window([HwResource::mmio(0x1000_0000, 0)].iter()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn sole_framebuffer_resolves_the_surface_and_fails_closed() {
        let mode = crate::driver::display::DisplayMode {
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: crate::driver::display::DisplayFormat::Bgra8888,
        };
        let fb = HwResource::framebuffer(0x4000_0000, &mode).expect("valid mode");
        // The surface resolves alongside unrelated grants.
        let grants = [HwResource::irq(33, 1), fb];
        assert_eq!(sole_framebuffer(grants.iter()), Ok((0x4000_0000, mode)));
        // No surface grant.
        assert_eq!(
            sole_framebuffer([HwResource::mmio(0x1000_0000, 0x1000)].iter()),
            Err(DriverError::NotFound)
        );
        // Two surface grants — ambiguous, refused rather than guessed.
        assert_eq!(
            sole_framebuffer([fb, fb].iter()),
            Err(DriverError::Unsupported)
        );
    }
}
