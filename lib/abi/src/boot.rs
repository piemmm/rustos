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
//!
//! [`BootFacts`] is the second per-boot value this module carries: the
//! kernel-attested, boot-static machine summary — the CPU architecture, the
//! boot CPU's discovered model name ([`CpuName`]), the number of processor
//! cores brought under the scheduler, and the installed physical memory the
//! boot path discovered. Like the boot id it is not a
//! secret (any task can measure its own timing across cores; the figures are
//! the machine's public shape, not its state), so it is exposed read-only to
//! any task through the ungated `boot_facts_get` syscall. Unlike the live
//! `sysinfo` figures it never changes after boot, carries no per-process or
//! usage detail, and grants no authority.

use crate::le::{read_u16, read_u32, read_u64};

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

/// Length, in bytes, of the [`BootFacts`] wire encoding.
pub const BOOT_FACTS_WIRE_LEN: usize = 16 + CPU_NAME_LEN;

/// Length, in bytes, of the [`CpuName`] wire field.
///
/// Exactly the 48 bytes of the x86 CPUID processor-brand string (leaves
/// `0x8000_0002..=0x8000_0004`, Intel SDM Vol. 2A), the longest CPU-name
/// source any Tier-1 target reports, so no discovered name is ever
/// truncated to fit the wire.
pub const CPU_NAME_LEN: usize = 48;

/// The human-readable model name of the boot CPU, as discovered by the
/// architecture port (`ARM Cortex-A72`, an x86 CPUID brand string,
/// `SiFive U74-MC`, …).
///
/// A bounded, NUL-padded UTF-8 string of at most [`CPU_NAME_LEN`] bytes.
/// The canonical form is enforced at construction and decode: no control
/// characters, no leading or trailing whitespace, and every byte after the
/// first NUL is NUL. The all-zero [`CpuName::UNKNOWN`] value means the port
/// could not derive a name — an honest "unknown", never a fabricated one —
/// and readers render their own fallback for it.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct CpuName([u8; CPU_NAME_LEN]);

impl CpuName {
    /// The reserved all-zero value: the port could not derive a CPU name.
    pub const UNKNOWN: Self = Self([0u8; CPU_NAME_LEN]);

    /// Construct a [`CpuName`] from a discovered name string.
    ///
    /// Returns `None` — so the caller falls back to
    /// [`UNKNOWN`](Self::UNKNOWN) rather than shipping a malformed name —
    /// when `name` is empty, longer than [`CPU_NAME_LEN`] bytes, contains a
    /// control character (including NUL), or carries leading or trailing
    /// whitespace (the discoverer trims; the wire form is canonical).
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > CPU_NAME_LEN
            || name.trim() != name
            || name.chars().any(char::is_control)
        {
            return None;
        }
        let mut buf = [0u8; CPU_NAME_LEN];
        buf[..bytes.len()].copy_from_slice(bytes);
        Some(Self(buf))
    }

    /// The name as a string, or `None` for [`UNKNOWN`](Self::UNKNOWN).
    ///
    /// Always `Some` for a value built by [`new`](Self::new) or
    /// decoded by [`from_wire`](Self::from_wire); the UTF-8 re-check exists
    /// only so a corrupted in-memory value degrades to "unknown" instead of
    /// panicking.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(CPU_NAME_LEN);
        if len == 0 {
            return None;
        }
        core::str::from_utf8(&self.0[..len]).ok()
    }

    /// The raw NUL-padded wire bytes.
    #[must_use]
    pub const fn to_wire(self) -> [u8; CPU_NAME_LEN] {
        self.0
    }

    /// Decode and validate the wire field.
    ///
    /// Fails closed with [`Errno::BadMagic`](crate::Errno::BadMagic) on any
    /// non-canonical image: a non-NUL byte after the first NUL, invalid
    /// UTF-8, a control character, or leading/trailing whitespace. The
    /// all-zero image decodes to [`UNKNOWN`](Self::UNKNOWN).
    pub fn from_wire(bytes: &[u8; CPU_NAME_LEN]) -> crate::Result<Self> {
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(CPU_NAME_LEN);
        if bytes[len..].iter().any(|&b| b != 0) {
            return Err(crate::Errno::BadMagic);
        }
        if len == 0 {
            return Ok(Self::UNKNOWN);
        }
        let name = core::str::from_utf8(&bytes[..len]).map_err(|_| crate::Errno::BadMagic)?;
        Self::new(name).ok_or(crate::Errno::BadMagic)
    }
}

impl core::fmt::Debug for CpuName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.as_str() {
            Some(name) => write!(f, "CpuName({name:?})"),
            None => f.write_str("CpuName(<unknown>)"),
        }
    }
}

/// The CPU architecture a TAIRiX kernel was built for.
///
/// A closed set: exactly the Tier-1 targets. The discriminants and the
/// canonical [`names`](Self::name) are part of the `abi-v1` contract; a wire
/// value outside the set fails closed at decode rather than being guessed.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Arch {
    /// 64-bit x86 (`x86_64-unknown-none`).
    X86_64 = 1,
    /// 64-bit Arm (`aarch64-unknown-none`).
    Aarch64 = 2,
    /// 64-bit RISC-V (`riscv64gc-unknown-none-elf`).
    Riscv64 = 3,
    /// WebAssembly (`wasm32-unknown-unknown`).
    Wasm32 = 4,
}

impl Arch {
    /// The wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a wire discriminant; `None` for a value outside the closed set.
    #[must_use]
    pub const fn from_u16(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::X86_64),
            2 => Some(Self::Aarch64),
            3 => Some(Self::Riscv64),
            4 => Some(Self::Wasm32),
            _ => None,
        }
    }

    /// The canonical display name (`x86_64`, `aarch64`, `riscv64`, `wasm32`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Riscv64 => "riscv64",
            Self::Wasm32 => "wasm32",
        }
    }
}

/// The kernel-attested, boot-static machine summary the ungated
/// `boot_facts_get` syscall reports.
///
/// Minted once by the kernel at boot from state it alone owns — the arch
/// port's identity, the scheduler's brought-up CPU count, and the boot
/// path's discovered physical-RAM total — and immutable thereafter. Not a
/// secret and not live state: usage figures, per-process detail, and
/// anything that changes after boot stay behind the capability-gated
/// System Information API.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BootFacts {
    /// The CPU architecture the running kernel was built for.
    pub arch: Arch,
    /// The boot CPU's discovered model name, or [`CpuName::UNKNOWN`] when
    /// the port could not derive one (readers render their own fallback).
    pub cpu_name: CpuName,
    /// Processor cores brought under the scheduler at boot. Never zero:
    /// at least the boot CPU runs.
    pub cpu_count: u32,
    /// Installed physical memory in bytes, as discovered by the boot
    /// path's platform memory source (firmware map / device tree / host
    /// query) **before** any kernel carve-outs. Never zero.
    pub memory_bytes: u64,
}

impl BootFacts {
    /// Length, in bytes, of the wire encoding.
    pub const WIRE_LEN: usize = BOOT_FACTS_WIRE_LEN;

    /// The wire encoding: `arch:u16`, `reserved:u16` (zero),
    /// `cpu_count:u32`, `memory_bytes:u64` (all little-endian), then the
    /// [`CPU_NAME_LEN`]-byte NUL-padded [`CpuName`].
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; BOOT_FACTS_WIRE_LEN] {
        let mut out = [0u8; BOOT_FACTS_WIRE_LEN];
        out[0..2].copy_from_slice(&self.arch.as_u16().to_le_bytes());
        // Bytes 2..4 are the reserved word, kept zero.
        out[4..8].copy_from_slice(&self.cpu_count.to_le_bytes());
        out[8..16].copy_from_slice(&self.memory_bytes.to_le_bytes());
        out[16..].copy_from_slice(&self.cpu_name.to_wire());
        out
    }

    /// Decode and validate a wire encoding.
    ///
    /// # Errors
    ///
    /// Fails closed: [`Errno::LengthOutOfRange`](crate::Errno) for a slice
    /// that is not exactly [`WIRE_LEN`](Self::WIRE_LEN) long,
    /// [`Errno::BadMagic`](crate::Errno) for an unknown arch discriminant,
    /// a non-zero reserved word, or a non-canonical CPU-name field, and
    /// [`Errno::OutOfRange`](crate::Errno) for a zero CPU count or zero
    /// memory size (no machine that boots has either).
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() != BOOT_FACTS_WIRE_LEN {
            return Err(crate::Errno::LengthOutOfRange);
        }
        let arch = Arch::from_u16(read_u16(bytes, 0)).ok_or(crate::Errno::BadMagic)?;
        if read_u16(bytes, 2) != 0 {
            return Err(crate::Errno::BadMagic);
        }
        let cpu_count = read_u32(bytes, 4);
        let memory_bytes = read_u64(bytes, 8);
        if cpu_count == 0 || memory_bytes == 0 {
            return Err(crate::Errno::OutOfRange);
        }
        let mut name = [0u8; CPU_NAME_LEN];
        name.copy_from_slice(&bytes[16..]);
        let cpu_name = CpuName::from_wire(&name)?;
        Ok(Self {
            arch,
            cpu_name,
            cpu_count,
            memory_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Arch, BootFacts, BootId, CpuName, BOOT_FACTS_WIRE_LEN, BOOT_ID_HEX_LEN, BOOT_ID_LEN,
        CPU_NAME_LEN,
    };
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

    #[test]
    fn arch_round_trips_and_rejects_unknown() {
        for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64, Arch::Wasm32] {
            assert_eq!(Arch::from_u16(arch.as_u16()), Some(arch));
        }
        assert_eq!(Arch::from_u16(0), None);
        assert_eq!(Arch::from_u16(5), None);
        assert_eq!(Arch::from_u16(u16::MAX), None);
    }

    #[test]
    fn arch_names_are_frozen() {
        assert_eq!(Arch::X86_64.name(), "x86_64");
        assert_eq!(Arch::Aarch64.name(), "aarch64");
        assert_eq!(Arch::Riscv64.name(), "riscv64");
        assert_eq!(Arch::Wasm32.name(), "wasm32");
    }

    #[test]
    fn boot_facts_round_trip() {
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: CpuName::new("ARM Cortex-A72").expect("valid name"),
            cpu_count: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
        };
        assert_eq!(BootFacts::WIRE_LEN, BOOT_FACTS_WIRE_LEN);
        assert_eq!(BootFacts::from_bytes(&facts.to_le_bytes()), Ok(facts));
        // An unknown CPU name round-trips too (the all-zero field).
        let unknown = BootFacts {
            cpu_name: CpuName::UNKNOWN,
            ..facts
        };
        assert_eq!(BootFacts::from_bytes(&unknown.to_le_bytes()), Ok(unknown));
    }

    #[test]
    fn cpu_name_construction_is_canonical() {
        let name = CpuName::new("ARM Cortex-A72").expect("valid name");
        assert_eq!(name.as_str(), Some("ARM Cortex-A72"));
        // The longest permitted name (exactly CPU_NAME_LEN bytes) fits.
        let max = "x".repeat(CPU_NAME_LEN);
        assert_eq!(
            CpuName::new(&max).expect("fits").as_str(),
            Some(max.as_str())
        );
        // Rejections: empty, too long, control characters, outer whitespace.
        assert_eq!(CpuName::new(""), None);
        assert_eq!(CpuName::new(&"x".repeat(CPU_NAME_LEN + 1)), None);
        assert_eq!(CpuName::new("bad\u{0}name"), None);
        assert_eq!(CpuName::new("bad\nname"), None);
        assert_eq!(CpuName::new(" padded"), None);
        assert_eq!(CpuName::new("padded "), None);
        // The unknown sentinel has no string.
        assert_eq!(CpuName::UNKNOWN.as_str(), None);
    }

    #[test]
    fn cpu_name_wire_decode_fails_closed() {
        // A non-NUL byte after the first NUL is a non-canonical image.
        let mut bad = [0u8; CPU_NAME_LEN];
        bad[0] = b'A';
        bad[2] = b'B';
        assert_eq!(CpuName::from_wire(&bad), Err(Errno::BadMagic));
        // Invalid UTF-8 is refused.
        let mut bad = [0u8; CPU_NAME_LEN];
        bad[0] = 0xff;
        assert_eq!(CpuName::from_wire(&bad), Err(Errno::BadMagic));
        // Trailing whitespace is non-canonical.
        let mut bad = [0u8; CPU_NAME_LEN];
        bad[..2].copy_from_slice(b"A ");
        assert_eq!(CpuName::from_wire(&bad), Err(Errno::BadMagic));
        // The all-zero image is the honest unknown, not an error.
        assert_eq!(
            CpuName::from_wire(&[0u8; CPU_NAME_LEN]),
            Ok(CpuName::UNKNOWN)
        );
    }

    #[test]
    fn boot_facts_decode_fails_closed() {
        let good = BootFacts {
            arch: Arch::X86_64,
            cpu_name: CpuName::new("Intel(R) Xeon(R)").expect("valid name"),
            cpu_count: 36,
            memory_bytes: 1 << 33,
        }
        .to_le_bytes();
        // Wrong length.
        assert_eq!(
            BootFacts::from_bytes(&good[..BOOT_FACTS_WIRE_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        // Unknown arch discriminant.
        let mut bad = good;
        bad[0] = 0xff;
        assert_eq!(BootFacts::from_bytes(&bad), Err(Errno::BadMagic));
        // Non-zero reserved word.
        let mut bad = good;
        bad[2] = 1;
        assert_eq!(BootFacts::from_bytes(&bad), Err(Errno::BadMagic));
        // Zero CPU count.
        let mut bad = good;
        bad[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(BootFacts::from_bytes(&bad), Err(Errno::OutOfRange));
        // Zero memory size.
        let mut bad = good;
        bad[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(BootFacts::from_bytes(&bad), Err(Errno::OutOfRange));
        // Non-canonical CPU-name field (a byte set after its first NUL).
        let mut bad = good;
        bad[BOOT_FACTS_WIRE_LEN - 1] = b'!';
        assert_eq!(BootFacts::from_bytes(&bad), Err(Errno::BadMagic));
    }
}
