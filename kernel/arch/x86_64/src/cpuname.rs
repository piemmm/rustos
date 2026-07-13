//! Boot-CPU model-name discovery for x86_64.
//!
//! Every x86_64 part reports its own marketing name — the 48-byte
//! processor brand string — through CPUID leaves
//! `0x8000_0002..=0x8000_0004` (Intel SDM Vol. 2A "CPUID — Processor
//! Brand String"; AMD64 APM Vol. 3), sixteen bytes per leaf in
//! `EAX`/`EBX`/`ECX`/`EDX` order, little-endian within each register.
//!
//! The decoder (`brand_from_bytes`) is pure and host-testable; the CPUID
//! reads only execute on the bare-metal target and the host build reports
//! `None` rather than the host machine's identity (no fake hardware in
//! production paths). A part that does not implement the brand-string
//! leaves, or reports an empty or non-UTF-8 string, is `None` — an honest
//! "unknown" the boot facts record as such.

/// Byte length of the CPUID processor brand string (three leaves of four
/// 32-bit registers).
pub const BRAND_LEN: usize = 48;

/// First CPUID leaf of the processor brand string. Only read on the
/// bare-metal target after bounding the maximum extended leaf.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_BRAND_FIRST: u32 = 0x8000_0002;

/// Last CPUID leaf of the processor brand string.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_BRAND_LAST: u32 = 0x8000_0004;

/// CPUID leaf bounding the maximum supported *extended* leaf, returned in
/// `EAX`. The brand-string leaves are read only when within bounds.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_EXT_MAX: u32 = 0x8000_0000;

/// Decode the raw 48 brand-string bytes into the part's name.
///
/// The hardware pads the string with NULs and vendors routinely pad the
/// *text* with leading spaces to right-justify it, so the string is cut
/// at the first NUL and trimmed of outer whitespace. Returns `None` for
/// an empty or non-UTF-8 result — the caller records an honest
/// "unknown", never a fabricated name.
#[must_use]
pub fn brand_from_bytes(bytes: &[u8; BRAND_LEN]) -> Option<&str> {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(BRAND_LEN);
    let name = core::str::from_utf8(&bytes[..len]).ok()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Read the brand string of the CPU this function executes on into
/// `buf`, returning the decoded name borrowed from it.
///
/// On the bare-metal target it bounds the maximum extended leaf via leaf
/// `0x8000_0000` (so an unsupported leaf is never executed, per the SDM
/// usage requirement), reads the three brand-string leaves, and decodes
/// them through [`brand_from_bytes`]. On the host target it reports
/// `None` rather than the host machine's identity.
#[must_use]
pub fn boot_cpu_name(buf: &mut [u8; BRAND_LEN]) -> Option<&str> {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // `CPUID` is unconditionally available on every x86_64 CPU (it
        // predates the architecture) and is side-effect-free, so these
        // intrinsics are safe on this target.
        let max_ext_leaf = core::arch::x86_64::__cpuid(LEAF_EXT_MAX).eax;
        if max_ext_leaf < LEAF_BRAND_LAST {
            return None;
        }
        for (i, leaf) in (LEAF_BRAND_FIRST..=LEAF_BRAND_LAST).enumerate() {
            let regs = core::arch::x86_64::__cpuid(leaf);
            let at = i * 16;
            buf[at..at + 4].copy_from_slice(&regs.eax.to_le_bytes());
            buf[at + 4..at + 8].copy_from_slice(&regs.ebx.to_le_bytes());
            buf[at + 8..at + 12].copy_from_slice(&regs.ecx.to_le_bytes());
            buf[at + 12..at + 16].copy_from_slice(&regs.edx.to_le_bytes());
        }
        brand_from_bytes(buf)
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        let _ = buf;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `BRAND_LEN` buffer holding `text` followed by NUL padding.
    fn padded(text: &str) -> [u8; BRAND_LEN] {
        let mut buf = [0u8; BRAND_LEN];
        buf[..text.len()].copy_from_slice(text.as_bytes());
        buf
    }

    #[test]
    fn brand_string_is_cut_at_nul_and_trimmed() {
        let buf = padded("  Intel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz");
        assert_eq!(
            brand_from_bytes(&buf),
            Some("Intel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz")
        );
        // A full 48-byte string with no NUL terminator decodes whole.
        let buf = padded(&"x".repeat(BRAND_LEN));
        assert_eq!(brand_from_bytes(&buf), Some("x".repeat(BRAND_LEN).as_str()));
    }

    #[test]
    fn empty_or_malformed_brand_is_none() {
        // All-NUL: a part without the brand-string leaves wired.
        assert_eq!(brand_from_bytes(&[0u8; BRAND_LEN]), None);
        // Whitespace-only text trims to nothing.
        assert_eq!(brand_from_bytes(&padded("   ")), None);
        // Non-UTF-8 bytes are refused, never lossily replaced.
        let mut buf = padded("ok");
        buf[0] = 0xff;
        assert_eq!(brand_from_bytes(&buf), None);
    }

    #[test]
    fn host_read_reports_none() {
        // The host build must not report the host machine's identity.
        let mut buf = [0u8; BRAND_LEN];
        assert_eq!(boot_cpu_name(&mut buf), None);
    }
}
