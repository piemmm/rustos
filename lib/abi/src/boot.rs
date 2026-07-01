//! Per-boot identity carried across the ABI.
//!
//! A [`BootId`] is a 128-bit value the kernel mints **once per boot** from its
//! single cryptographic random subsystem. It is stable for the lifetime of a
//! boot and fresh across boots: two boots of the same installation never share
//! a `BootId` (with overwhelming probability), and user space can neither
//! supply nor influence it.
//!
//! The value is not a secret — it is a public per-boot nonce. Its purpose is to
//! bind boot-scoped state to the boot that produced it: the system log binds
//! each stream's hash-chain genesis to `machine-id-hash`, the stream, and the
//! `BootId` (`plans/SYSLOG.md` §7.1), and signed anchors record it (§7.3), so a
//! log segment cannot be silently replayed from a different boot. Because it is
//! not secret, it is exposed read-only to any task through the `boot_id_get`
//! syscall.
//!
//! The 16-byte width and the all-zero [`BootId::UNSET`] sentinel are part of
//! the `abi-v1` contract.

/// Length, in bytes, of a [`BootId`].
pub const BOOT_ID_LEN: usize = 16;

/// Length, in bytes, of the lowercase-hex rendering of a [`BootId`].
pub const BOOT_ID_HEX_LEN: usize = BOOT_ID_LEN * 2;

/// A kernel-generated 128-bit per-boot identifier.
///
/// Opaque by construction: the bytes carry no caller-meaningful structure and
/// must be treated as a single value. Equality and ordering are byte-wise so
/// the value can be compared and rendered stably.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BootId([u8; BOOT_ID_LEN]);

impl BootId {
    /// The reserved all-zero identifier.
    ///
    /// Denotes "no boot id has been minted yet". The kernel mints a `BootId`
    /// only from random bytes it actually drew, so it never deliberately
    /// produces this value for a live boot; a reader that observes
    /// [`BootId::UNSET`] therefore knows the per-boot identity was not
    /// available (the random subsystem was not seeded), and must fail closed
    /// rather than treat all-zero as a real id.
    pub const UNSET: Self = Self([0u8; BOOT_ID_LEN]);

    /// Construct a [`BootId`] from its raw 16 bytes.
    ///
    /// The bytes are taken verbatim; this is the kernel-side minter's
    /// constructor, not a user-reachable path.
    #[must_use]
    pub const fn from_raw(bytes: [u8; BOOT_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; BOOT_ID_LEN] {
        &self.0
    }

    /// The on-wire encoding (the raw bytes, which are endian-neutral).
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; BOOT_ID_LEN] {
        self.0
    }

    /// Decode a [`BootId`] from a byte slice.
    ///
    /// Returns [`Errno::LengthOutOfRange`](crate::Errno::LengthOutOfRange) if
    /// `bytes` is not exactly [`BOOT_ID_LEN`] long — never silently truncating
    /// or zero-extending a malformed input (fail closed).
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() != BOOT_ID_LEN {
            return Err(crate::Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; BOOT_ID_LEN];
        buf.copy_from_slice(bytes);
        Ok(Self(buf))
    }

    /// `true` if this is the [`UNSET`](Self::UNSET) sentinel.
    #[must_use]
    pub fn is_unset(self) -> bool {
        self == Self::UNSET
    }

    /// Render the identifier as lowercase hexadecimal into `out`.
    ///
    /// Allocation-free: the caller supplies the fixed-size destination so the
    /// rendering runs in `no_std` contexts that must not allocate. The
    /// returned `&str` borrows `out`.
    #[must_use]
    pub fn write_hex(self, out: &mut [u8; BOOT_ID_HEX_LEN]) -> &str {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut i = 0;
        while i < BOOT_ID_LEN {
            out[i * 2] = DIGITS[(self.0[i] >> 4) as usize];
            out[i * 2 + 1] = DIGITS[(self.0[i] & 0x0f) as usize];
            i += 1;
        }
        // Every byte written above is an ASCII hex digit, so `out` is valid
        // UTF-8; fall back to the empty string rather than panic.
        core::str::from_utf8(out).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::{BootId, BOOT_ID_HEX_LEN, BOOT_ID_LEN};
    use crate::Errno;

    #[test]
    fn unset_sentinel_is_all_zero_and_recognised() {
        assert_eq!(BootId::UNSET.as_bytes(), &[0u8; BOOT_ID_LEN]);
        assert!(BootId::UNSET.is_unset());
        assert!(!BootId::from_raw([1u8; BOOT_ID_LEN]).is_unset());
    }

    #[test]
    fn round_trips_through_bytes() {
        let bytes = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let id = BootId::from_raw(bytes);
        assert_eq!(id.to_le_bytes(), bytes);
        assert_eq!(BootId::from_bytes(&id.to_le_bytes()), Ok(id));
    }

    #[test]
    fn from_bytes_rejects_wrong_length_fail_closed() {
        assert_eq!(BootId::from_bytes(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(
            BootId::from_bytes(&[0u8; BOOT_ID_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            BootId::from_bytes(&[0u8; BOOT_ID_LEN + 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn write_hex_is_lowercase_and_exact() {
        let bytes = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut buf = [0u8; BOOT_ID_HEX_LEN];
        let rendered = BootId::from_raw(bytes).write_hex(&mut buf);
        assert_eq!(rendered, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn distinct_values_compare_unequal() {
        assert_ne!(
            BootId::from_raw([1u8; BOOT_ID_LEN]),
            BootId::from_raw([2u8; BOOT_ID_LEN])
        );
    }
}
