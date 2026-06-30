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
//! The layout is part of the frozen `abi-v1` contract: the 16-byte width and
//! the all-zero [`ProcId::KERNEL`] sentinel must not change in place.

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

#[cfg(test)]
mod tests {
    use super::{ProcId, PROC_ID_HEX_LEN, PROC_ID_LEN};
    use crate::Errno;

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
}
