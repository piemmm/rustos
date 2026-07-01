//! Process-instance identity carried across the ABI.
//!
//! A [`ProcId`] is a kernel-generated 128-bit identifier assigned to a
//! process instance when it is admitted. It is **not** the reusable numeric
//! PID: the kernel hands out PIDs from a small recycled space, so two process
//! lifetimes can share a PID, but they never share a `ProcId`. Security
//! attribution (the hash-chained audit log) and any future origin record can
//! therefore distinguish "the login that ran as PID 42 this morning" from "the
//! shell that reused PID 42 this afternoon" without ambiguity.
//!
//! The value is generated entirely kernel-side from the single kernel random
//! subsystem mixed with a monotonic per-boot counter; user space never
//! supplies or influences it, so a caller can neither forge another instance's
//! identity nor predict its own ahead of admission. A process instance can
//! only ever observe its own `ProcId`, never mint one.
//!
//! The 16-byte width and the all-zero [`ProcId::KERNEL`] sentinel are part of
//! the `abi-v1` contract.
//!
//! This module also defines the kernel-attested [`Origin`] record — the
//! authoritative identity of the principal that performed an action, built
//! entirely from kernel state and never from caller-supplied bytes — together
//! with its [`TrustDomain`] classification and the non-secret
//! [`CapabilitySummary`] it carries.

use crate::capability::{CapabilityId, CapabilityQuery};
use crate::le::{put_u32, put_u64, read_u32, read_u64};

/// Length, in bytes, of a [`ProcId`].
pub const PROC_ID_LEN: usize = 16;

/// Length, in bytes, of the lowercase-hex rendering of a [`ProcId`].
pub const PROC_ID_HEX_LEN: usize = PROC_ID_LEN * 2;

/// A kernel-generated 128-bit process-instance identifier.
///
/// Opaque by construction: the bytes carry no caller-meaningful structure and
/// must be treated as a single unforgeable token. Equality and ordering are
/// byte-wise so the value can key a registry or sort stably in a listing.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ProcId([u8; PROC_ID_LEN]);

impl ProcId {
    /// The reserved all-zero identifier.
    ///
    /// Denotes a schedulable entity that is **not** a distinct user process
    /// instance — the kernel's own threads and the in-kernel capability
    /// records for IPC binders and device hosts, which share the kernel trust
    /// domain. The minter never produces this value for a real process (its
    /// monotonic counter starts at 1), so a zero `ProcId` unambiguously means
    /// "no process instance".
    pub const KERNEL: Self = Self([0u8; PROC_ID_LEN]);

    /// Construct a [`ProcId`] from its raw 16 bytes.
    ///
    /// The bytes are taken verbatim; this is the kernel-side minter's
    /// constructor, not a user-reachable path.
    #[must_use]
    pub const fn from_raw(bytes: [u8; PROC_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; PROC_ID_LEN] {
        &self.0
    }

    /// The on-wire encoding (the raw bytes, which are endian-neutral).
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; PROC_ID_LEN] {
        self.0
    }

    /// Decode a [`ProcId`] from a byte slice.
    ///
    /// Returns [`Errno::LengthOutOfRange`](crate::Errno::LengthOutOfRange) if
    /// `bytes` is not exactly [`PROC_ID_LEN`] long — never silently truncating
    /// or zero-extending a malformed input (fail closed).
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() != PROC_ID_LEN {
            return Err(crate::Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; PROC_ID_LEN];
        buf.copy_from_slice(bytes);
        Ok(Self(buf))
    }

    /// `true` if this is the [`KERNEL`](Self::KERNEL) sentinel.
    #[must_use]
    pub fn is_kernel(self) -> bool {
        self == Self::KERNEL
    }

    /// Render the identifier as lowercase hexadecimal into `out`.
    ///
    /// Allocation-free: the caller supplies the fixed-size destination so the
    /// rendering runs in the kernel's audit path (which is `no_std` and must
    /// not allocate). The returned `&str` borrows `out`.
    #[must_use]
    pub fn write_hex(self, out: &mut [u8; PROC_ID_HEX_LEN]) -> &str {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut i = 0;
        while i < PROC_ID_LEN {
            out[i * 2] = DIGITS[(self.0[i] >> 4) as usize];
            out[i * 2 + 1] = DIGITS[(self.0[i] & 0x0f) as usize];
            i += 1;
        }
        // SAFETY: every byte written above is an ASCII hex digit, so `out`
        // is valid UTF-8.
        core::str::from_utf8(out).unwrap_or("")
    }
}

/// The trust class of a process instance, as attested by the kernel.
///
/// This is the kernel's honest classification of *what kind of principal*
/// acted — the coarse domain a security consumer (the journal ingress, an
/// audit reader) uses to bucket an action. The kernel attests it from state
/// it actually holds, never from anything the caller supplies.
///
/// Only the distinctions the kernel can make correctly today are encoded:
/// whether the schedulable entity is the kernel itself ([`Self::Kernel`], the
/// [`ProcId::KERNEL`] sentinel) or a distinct user process instance
/// ([`Self::User`]). Finer classes (driver vs. system service vs. application)
/// require executable-role metadata the kernel does not yet record; they are
/// added in place here when that producer exists, so no variant is defined
/// ahead of a source that can attest it.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum TrustDomain {
    /// The kernel trust domain: a kernel thread, or an in-kernel capability
    /// record for an IPC binder or device host. Carries the
    /// [`ProcId::KERNEL`] sentinel.
    Kernel = 0,
    /// A distinct user process instance, carrying a minted [`ProcId`].
    User = 1,
}

impl TrustDomain {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a [`TrustDomain`] from its wire discriminant.
    ///
    /// Returns [`Errno::OutOfRange`](crate::Errno::OutOfRange) for any value
    /// that is not a defined variant — never inventing a domain (fail closed).
    pub const fn from_u8(raw: u8) -> crate::Result<Self> {
        match raw {
            0 => Ok(Self::Kernel),
            1 => Ok(Self::User),
            _ => Err(crate::Errno::OutOfRange),
        }
    }
}

/// Length, in bytes, of a [`CapabilitySummary`] — a 256-bit membership bitmap.
///
/// Matches the wire image of the kernel's `CapabilitySet` (`lib/caps`) bit for
/// bit, so the kernel can fill a summary by copying that image verbatim.
pub const CAPABILITY_SUMMARY_LEN: usize = 32;

/// A non-secret, fixed-size summary of the capabilities a principal holds.
///
/// This is a **membership bitmap** of [`CapabilityId`]s — bit `id` is set iff
/// the principal's effective set holds capability `id` — and carries **no**
/// unforgeable capability *tokens*. A reader can therefore learn *which*
/// authorities a principal had without gaining any of them, which is exactly
/// what an audit/origin consumer needs and all it is permitted to see.
///
/// The bit at capability `id` lives in byte `id / 8`, bit `id % 8`, identical
/// to the kernel `CapabilitySet` wire image, so the kernel attests a summary
/// by copying that image with no re-encoding.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct CapabilitySummary([u8; CAPABILITY_SUMMARY_LEN]);

impl CapabilitySummary {
    /// The empty summary: a principal holding no capabilities.
    pub const EMPTY: Self = Self([0u8; CAPABILITY_SUMMARY_LEN]);

    /// Wrap a raw 256-bit bitmap (the kernel `CapabilitySet` wire image).
    #[must_use]
    pub const fn from_raw(bytes: [u8; CAPABILITY_SUMMARY_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bitmap bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; CAPABILITY_SUMMARY_LEN] {
        &self.0
    }

    /// Record that capability `cap` is held.
    ///
    /// Allocation-free builder used host-side and in tests; the kernel
    /// normally constructs a summary by copying a `CapabilitySet` image
    /// wholesale via [`from_raw`](Self::from_raw).
    pub fn insert(&mut self, cap: CapabilityId) {
        let index = cap.index();
        self.0[index / 8] |= 1u8 << (index % 8);
    }

    /// `true` if capability `cap` is recorded as held.
    #[must_use]
    pub fn holds_cap(&self, cap: CapabilityId) -> bool {
        let index = cap.index();
        (self.0[index / 8] >> (index % 8)) & 1 == 1
    }
}

impl CapabilityQuery for CapabilitySummary {
    fn holds(&self, cap: CapabilityId) -> bool {
        self.holds_cap(cap)
    }
}

/// Length, in bytes, of the [`Origin`] wire encoding.
pub const ORIGIN_WIRE_LEN: usize = 1 + 4 + 4 + 8 + PROC_ID_LEN + CAPABILITY_SUMMARY_LEN;

/// The kernel-attested identity of a principal that performed an action.
///
/// An `Origin` answers "who really did this?" with values the **kernel**
/// vouches for: it is filled entirely from the acting task's own kernel state,
/// never from anything the caller put on the wire. A security consumer (the
/// System Information self-identity query today, the journal ingress later)
/// can therefore trust it as authoritative — a caller can neither forge
/// another principal's origin nor inflate its own.
///
/// # Fields
///
/// The record carries what the kernel can attest correctly today: the
/// [`trust_domain`](Self::trust_domain), the owning [`uid`](Self::uid) and
/// primary [`gid`](Self::gid), the reusable numeric [`pid`](Self::pid), the
/// unforgeable [`proc_id`](Self::proc_id) that distinguishes process instances
/// across PID reuse, and a non-secret [`capabilities`](Self::capabilities)
/// summary. The `gid` is the primary group of the task's kernel-attested
/// credential, snapshotted at process creation from the identity table the
/// kernel vouches for (never caller-supplied). Parent pid, start time, and
/// executable identity are deliberately absent: the kernel does not yet record
/// them per task, and a field without a live producer would be a speculative
/// surface. They are added in place when their producer exists (the ABI is not
/// yet frozen), never as a parallel versioned type.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    trust_domain: TrustDomain,
    uid: u32,
    gid: u32,
    pid: u64,
    proc_id: ProcId,
    capabilities: CapabilitySummary,
}

impl Origin {
    /// Construct an `Origin` from its attested parts.
    ///
    /// This is the kernel-side attestation constructor; it has no
    /// user-reachable form, so the values can only ever be the ones the
    /// kernel filled in.
    #[must_use]
    pub const fn new(
        trust_domain: TrustDomain,
        uid: u32,
        gid: u32,
        pid: u64,
        proc_id: ProcId,
        capabilities: CapabilitySummary,
    ) -> Self {
        Self {
            trust_domain,
            uid,
            gid,
            pid,
            proc_id,
            capabilities,
        }
    }

    /// The principal's attested trust domain.
    #[must_use]
    pub const fn trust_domain(&self) -> TrustDomain {
        self.trust_domain
    }

    /// The owning user identifier.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// The primary group identifier of the task's attested credential.
    #[must_use]
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// The reusable numeric process identifier.
    #[must_use]
    pub const fn pid(&self) -> u64 {
        self.pid
    }

    /// The unforgeable process-instance identifier (distinct from
    /// [`pid`](Self::pid) across PID reuse).
    #[must_use]
    pub const fn proc_id(&self) -> ProcId {
        self.proc_id
    }

    /// The non-secret summary of the capabilities the principal holds.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilitySummary {
        &self.capabilities
    }

    /// Encode the `Origin` little-endian into a fixed-size buffer.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; ORIGIN_WIRE_LEN] {
        let mut out = [0u8; ORIGIN_WIRE_LEN];
        out[0] = self.trust_domain.as_u8();
        put_u32(&mut out, 1, self.uid);
        put_u32(&mut out, 5, self.gid);
        put_u64(&mut out, 9, self.pid);
        out[17..33].copy_from_slice(self.proc_id.as_bytes());
        out[33..65].copy_from_slice(self.capabilities.as_bytes());
        out
    }

    /// Decode an `Origin` from a byte slice.
    ///
    /// Fails closed: returns [`Errno::LengthOutOfRange`](crate::Errno::LengthOutOfRange)
    /// if `bytes` is not exactly [`ORIGIN_WIRE_LEN`] long, and
    /// [`Errno::OutOfRange`](crate::Errno::OutOfRange) if the trust-domain
    /// discriminant is not a defined variant — never guessing at a malformed
    /// record.
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() != ORIGIN_WIRE_LEN {
            return Err(crate::Errno::LengthOutOfRange);
        }
        let trust_domain = TrustDomain::from_u8(bytes[0])?;
        let uid = read_u32(bytes, 1);
        let gid = read_u32(bytes, 5);
        let pid = read_u64(bytes, 9);
        let proc_id = ProcId::from_bytes(&bytes[17..33])?;
        let mut caps = [0u8; CAPABILITY_SUMMARY_LEN];
        caps.copy_from_slice(&bytes[33..65]);
        Ok(Self {
            trust_domain,
            uid,
            gid,
            pid,
            proc_id,
            capabilities: CapabilitySummary::from_raw(caps),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilitySummary, Origin, ProcId, TrustDomain, ORIGIN_WIRE_LEN, PROC_ID_HEX_LEN,
        PROC_ID_LEN,
    };
    use crate::capability::CapabilityQuery;
    use crate::{CapabilityId, Errno};

    #[test]
    fn kernel_sentinel_is_all_zero_and_recognised() {
        assert_eq!(ProcId::KERNEL.as_bytes(), &[0u8; PROC_ID_LEN]);
        assert!(ProcId::KERNEL.is_kernel());
        assert!(!ProcId::from_raw([1u8; PROC_ID_LEN]).is_kernel());
    }

    #[test]
    fn round_trips_through_bytes() {
        let bytes = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let id = ProcId::from_raw(bytes);
        assert_eq!(id.to_le_bytes(), bytes);
        assert_eq!(ProcId::from_bytes(&id.to_le_bytes()), Ok(id));
    }

    #[test]
    fn from_bytes_rejects_wrong_length_fail_closed() {
        assert_eq!(ProcId::from_bytes(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(
            ProcId::from_bytes(&[0u8; PROC_ID_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ProcId::from_bytes(&[0u8; PROC_ID_LEN + 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn write_hex_is_lowercase_and_exact() {
        let bytes = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut buf = [0u8; PROC_ID_HEX_LEN];
        let rendered = ProcId::from_raw(bytes).write_hex(&mut buf);
        assert_eq!(rendered, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn kernel_sentinel_renders_all_zeros() {
        let mut buf = [0u8; PROC_ID_HEX_LEN];
        assert_eq!(
            ProcId::KERNEL.write_hex(&mut buf),
            "00000000000000000000000000000000"
        );
    }

    #[test]
    fn distinct_values_compare_unequal() {
        assert_ne!(
            ProcId::from_raw([1u8; PROC_ID_LEN]),
            ProcId::from_raw([2u8; PROC_ID_LEN])
        );
    }

    #[test]
    fn trust_domain_round_trips_and_rejects_unknown() {
        assert_eq!(TrustDomain::Kernel.as_u8(), 0);
        assert_eq!(TrustDomain::User.as_u8(), 1);
        assert_eq!(TrustDomain::from_u8(0), Ok(TrustDomain::Kernel));
        assert_eq!(TrustDomain::from_u8(1), Ok(TrustDomain::User));
        assert_eq!(TrustDomain::from_u8(2), Err(Errno::OutOfRange));
        assert_eq!(TrustDomain::from_u8(0xff), Err(Errno::OutOfRange));
    }

    #[test]
    fn capability_summary_records_membership_and_answers_query() {
        let mut summary = CapabilitySummary::EMPTY;
        assert!(!summary.holds_cap(CapabilityId::SYSINFO_GLOBAL));
        summary.insert(CapabilityId::SYSINFO_GLOBAL);
        summary.insert(CapabilityId::FS_ACCESS);
        assert!(summary.holds_cap(CapabilityId::SYSINFO_GLOBAL));
        assert!(summary.holds_cap(CapabilityId::FS_ACCESS));
        assert!(!summary.holds_cap(CapabilityId::NET_RAW));
        // The same answer through the object-safe seam the dispatcher gates on.
        let query: &dyn CapabilityQuery = &summary;
        assert!(query.holds(CapabilityId::SYSINFO_GLOBAL));
        assert!(!query.holds(CapabilityId::NET_RAW));
    }

    #[test]
    fn capability_summary_bit_layout_matches_index() {
        // Bit `id` lives in byte `id / 8`, bit `id % 8` — the kernel
        // `CapabilitySet` wire image the kernel copies in verbatim.
        let mut summary = CapabilitySummary::EMPTY;
        summary.insert(CapabilityId::SYSINFO_GLOBAL); // id 13
        let index = CapabilityId::SYSINFO_GLOBAL.index();
        assert_eq!(summary.as_bytes()[index / 8], 1u8 << (index % 8));
    }

    fn sample_origin() -> Origin {
        let mut caps = CapabilitySummary::EMPTY;
        caps.insert(CapabilityId::SYSINFO_GLOBAL);
        caps.insert(CapabilityId::FS_ACCESS);
        Origin::new(
            TrustDomain::User,
            1000,
            50,
            42,
            ProcId::from_raw([0xAB; PROC_ID_LEN]),
            caps,
        )
    }

    #[test]
    fn origin_round_trips_through_bytes() {
        let origin = sample_origin();
        let bytes = origin.to_le_bytes();
        assert_eq!(bytes.len(), ORIGIN_WIRE_LEN);
        let decoded = Origin::from_bytes(&bytes).expect("valid origin decodes");
        assert_eq!(decoded, origin);
        assert_eq!(decoded.trust_domain(), TrustDomain::User);
        assert_eq!(decoded.uid(), 1000);
        assert_eq!(decoded.gid(), 50);
        assert_eq!(decoded.pid(), 42);
        assert_eq!(decoded.proc_id(), ProcId::from_raw([0xAB; PROC_ID_LEN]));
        assert!(decoded
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
    }

    #[test]
    fn origin_from_bytes_rejects_wrong_length_fail_closed() {
        assert_eq!(Origin::from_bytes(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(
            Origin::from_bytes(&[0u8; ORIGIN_WIRE_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            Origin::from_bytes(&[0u8; ORIGIN_WIRE_LEN + 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn origin_from_bytes_rejects_unknown_trust_domain() {
        let mut bytes = sample_origin().to_le_bytes();
        bytes[0] = 7; // not a defined TrustDomain variant
        assert_eq!(Origin::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn kernel_origin_carries_the_kernel_sentinel() {
        let origin = Origin::new(
            TrustDomain::Kernel,
            0,
            0,
            1,
            ProcId::KERNEL,
            CapabilitySummary::EMPTY,
        );
        let decoded = Origin::from_bytes(&origin.to_le_bytes()).expect("decodes");
        assert_eq!(decoded.trust_domain(), TrustDomain::Kernel);
        assert!(decoded.proc_id().is_kernel());
    }
}
