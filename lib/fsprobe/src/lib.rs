//! Filesystem signature, label, and identity probe (`plans/DEVICES.md` D3c).
//!
//! A volume manager that has windowed a partition extent must decide, from
//! the extent's own first bytes, **which** supported filesystem lives there
//! before it can ask the kernel to attach it (`rustos_abi::volume`). This
//! crate is the one definition of that decision:
//!
//! * the on-disk **signatures** that identify a `RustFS`, ext4, or FAT32
//!   volume head (and the sanity bounds that keep a lookalike from
//!   matching),
//! * the stable 16-byte **identity** each format carries (the `RustFS`
//!   metadata-header UUID, the ext4 `s_uuid`, and the FAT32
//!   serial+label+tag derivation the FAT32 driver publishes), and
//! * the human-facing **volume label** where the format records one.
//!
//! The filesystem drivers import their shared constants and derivations
//! from here (`RUSTFS_HEADER_MAGIC`, `EXT4_SUPERBLOCK_MAGIC`,
//! [`fat32_identity_from_boot`]), so the probe and the mounted driver can
//! never disagree about what identifies a volume.
//!
//! The probe consumes **untrusted bytes** read straight from removable
//! media: every access is bounds-checked, every field is validated before
//! use, and anything that does not match a supported signature exactly is
//! `None` — never a guess.
//!
//! It also renders a stable identity as the short display **fingerprint**
//! (`plans/ALIAS.md` §3.8): the lowercase Crockford base32 encoding of the
//! identity bytes, of which a caller takes the prefix it needs (catalog
//! collision suffixes, alias identity guards).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_abi::volume::VolumeFsType;

/// Bytes of an extent head the probe needs to see every supported
/// signature: the FAT32 boot sector (512), the `RustFS` metadata header
/// (128), and the ext4 superblock, which occupies bytes 1024..2048.
///
/// A caller reads at least this many bytes from the start of the extent
/// (whole blocks, zero-padded when the extent is shorter) and hands them to
/// [`probe`].
pub const PROBE_HEAD_LEN: usize = 2048;

/// On-disk magic in the first eight bytes of every `RustFS` metadata block:
/// `"RUSTFSB\3"` little-endian. Defined here so the `RustFS` driver and this
/// probe share one value (`docs/src/filesystem/rustfs-spec.md` §8).
pub const RUSTFS_HEADER_MAGIC: u64 = 0x5255_5354_4653_4203;

/// ext4/ext3/ext2 superblock magic (`s_magic`), little-endian at byte
/// `0x38` of the superblock (fixed by the ext on-disk format).
pub const EXT4_SUPERBLOCK_MAGIC: u16 = 0xEF53;

/// Maximum bytes of a probed volume label: the ext family's
/// `s_volume_name` field is 16 bytes, the longest any supported format
/// records.
pub const LABEL_MAX: usize = 16;

/// Characters of the full identity fingerprint: 16 identity bytes = 128
/// bits, at five bits per base32 character, rounded up.
pub const FINGERPRINT_CHARS: usize = 26;

/// The result of recognising a supported filesystem at an extent head.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProbedVolume {
    /// The filesystem the signature identified.
    pub fstype: VolumeFsType,
    /// The volume's stable 16-byte identity, exactly as the matching
    /// filesystem driver publishes it when the volume is attached.
    pub identity: [u8; 16],
    label: [u8; LABEL_MAX],
    label_len: u8,
}

impl ProbedVolume {
    /// The volume's recorded human-facing label, trimmed of the format's
    /// padding; empty when the format records none (`RustFS`) or the field
    /// is blank.
    #[must_use]
    pub fn label(&self) -> &[u8] {
        &self.label[..usize::from(self.label_len)]
    }
}

/// Recognise a supported filesystem from the first bytes of an extent.
///
/// `head` is the extent's leading bytes ([`PROBE_HEAD_LEN`] or more for
/// full coverage; a shorter head simply cannot match the signatures that
/// lie beyond it). The probe order is fixed and documented: `RustFS`, then
/// ext4, then FAT32 — most-specific signature first, so a volume can never
/// be claimed by a weaker lookalike check. Returns `None` when nothing
/// matches (fail closed, never a guess).
#[must_use]
pub fn probe(head: &[u8]) -> Option<ProbedVolume> {
    probe_rustfs(head)
        .or_else(|| probe_ext4(head))
        .or_else(|| probe_fat32(head))
}

/// Ceiling on a mutation-evidence extent ([`evidence_len`]), in bytes: the
/// largest any supported format's declaration can reach (the `RustFS`
/// superblock ring of eight 4 KiB blocks). A head whose declared evidence
/// would exceed it is refused (`None`), never clamped — a clamped window
/// could no longer prove non-mutation.
pub const EVIDENCE_MAX: u64 = 32 * 1024;

/// Physical blocks of the `RustFS` superblock ring: four logical slots,
/// each a mirrored pair. Defined here (like [`RUSTFS_HEADER_MAGIC`]) so
/// the `RustFS` driver and the [`evidence_len`] window share one value.
pub const RUSTFS_RING_BLOCKS: u64 = 8;

/// The largest `RustFS` declaration (the ring at the format's maximum
/// 4 KiB block size) fits the ceiling, so a structurally valid head is
/// never refused for size alone.
const _: () = assert!(RUSTFS_RING_BLOCKS * 4096 <= EVIDENCE_MAX);

/// Byte length, from the start of the extent, of the region whose content
/// any *foreign mutation* of the volume must rewrite — the
/// mutation-evidence window the verified re-insert path compares
/// (`plans/DEVICES.md` D4c). One definition per format, beside the
/// signatures, so the probe and the verifier can never disagree:
///
/// * `RustFS`: the whole superblock ring ([`RUSTFS_RING_BLOCKS`] mirrored
///   slot blocks) — every committed transaction publishes a new generation
///   into a ring slot, so a foreign writer must land there.
/// * ext4: the first 2048 bytes — the primary superblock (bytes
///   1024..2048), whose write-time, mount count, and checksums a foreign
///   kernel updates on any read-write mount.
/// * FAT32: the reserved sectors through the `FSInfo` sector — a foreign
///   writer updates the `FSInfo` free-cluster fields (honestly weaker than
///   the other formats; the format offers nothing stronger).
///
/// `None` when `head` matches no supported signature or the declared
/// window is structurally implausible (fail closed — no evidence means no
/// replay, never a guess).
#[must_use]
pub fn evidence_len(head: &[u8]) -> Option<u64> {
    let len = match probe(head)?.fstype {
        VolumeFsType::RustFs => {
            // The plaintext block-size field of the superblock payload
            // (byte 128, following the metadata-block header). It is
            // unauthenticated here, but only sizes the window: a lying
            // value fails the format's power-of-two 512..=4096 bounds and
            // the whole declaration fails closed.
            let block_size = u64::from(le_u32(head, 128)?);
            if !block_size.is_power_of_two() || !(512..=4096).contains(&block_size) {
                return None;
            }
            RUSTFS_RING_BLOCKS * block_size
        }
        VolumeFsType::Ext4 => 2048,
        VolumeFsType::Fat32 => {
            let bytes_per_sector = u64::from(le_u16(head, 11)?);
            let fsinfo_sector = u64::from(le_u16(head, 48)?);
            let reserved_sectors = u64::from(le_u16(head, 14)?);
            // FSInfo lives in the reserved region; a boot sector that
            // points elsewhere is lying.
            if fsinfo_sector == 0 || fsinfo_sector >= reserved_sectors {
                return None;
            }
            (fsinfo_sector + 1) * bytes_per_sector
        }
    };
    (len <= EVIDENCE_MAX).then_some(len)
}

/// The stable FAT32 identity derived from a boot sector: the BPB volume
/// serial (4 bytes) + label (11 bytes) when the extended boot signature
/// (`0x29` at byte 66) declares them, else zeros, with a trailing tag byte
/// keeping the identity non-nil either way. FAT32 has no UUID;
/// `docs/src/filesystem/drives.md` §8 sanctions a content-derived identity
/// for formats without one. The FAT32 driver publishes exactly this value,
/// so a probe-side fingerprint always names the attached volume.
#[must_use]
pub fn fat32_identity_from_boot(boot: &[u8; 512]) -> [u8; 16] {
    let mut identity = [0u8; 16];
    if boot[66] == 0x29 {
        identity[..4].copy_from_slice(&boot[67..71]);
        identity[4..15].copy_from_slice(&boot[71..82]);
    }
    identity[15] = 0xF3;
    identity
}

/// Render `identity` as its full display fingerprint: lowercase Crockford
/// base32 (digits and letters, minus the look-alikes `i`, `l`, `o`, `u`),
/// most-significant bits first. A caller takes the prefix it needs; the
/// full [`FINGERPRINT_CHARS`] characters encode every identity bit, so two
/// distinct identities always have distinct full fingerprints.
#[must_use]
pub fn fingerprint(identity: &[u8; 16]) -> [u8; FINGERPRINT_CHARS] {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut out = [0u8; FINGERPRINT_CHARS];
    let mut acc: u32 = 0;
    let mut acc_bits: u32 = 0;
    let mut next = 0usize;
    for &byte in identity {
        acc = (acc << 8) | u32::from(byte);
        acc_bits += 8;
        while acc_bits >= 5 {
            acc_bits -= 5;
            out[next] = ALPHABET[usize::try_from((acc >> acc_bits) & 0x1F).unwrap_or(0)];
            next += 1;
        }
    }
    // 128 bits leave a 3-bit tail; pad it to a final character so every
    // identity bit is encoded.
    out[next] = ALPHABET[usize::try_from((acc << (5 - acc_bits)) & 0x1F).unwrap_or(0)];
    out
}

/// Little-endian `u16` at `offset`, or `None` past the end.
fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Little-endian `u32` at `offset`, or `None` past the end.
fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Little-endian `u64` at `offset`, or `None` past the end.
fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw = bytes.get(offset..offset.checked_add(8)?)?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(raw);
    Some(u64::from_le_bytes(buf))
}

/// Copy a fixed-width, padding-trimmed label field into a probe result's
/// label buffer. Trailing NULs and spaces are padding in both formats that
/// record labels; interior bytes are kept verbatim (the naming policy
/// above this crate decides what is renderable).
fn take_label(field: &[u8]) -> ([u8; LABEL_MAX], u8) {
    let mut label = [0u8; LABEL_MAX];
    let trimmed_len = field
        .iter()
        .rposition(|&b| b != 0 && b != b' ')
        .map_or(0, |last| last + 1);
    let take = trimmed_len.min(LABEL_MAX);
    label[..take].copy_from_slice(&field[..take]);
    #[allow(clippy::cast_possible_truncation)] // `take <= LABEL_MAX` (16).
    (label, take as u8)
}

/// `RustFS`: every metadata block opens with the header magic, and block 0
/// of a volume is a superblock-ring slot, so the head of a `RustFS` extent
/// always carries the magic followed by the block-type word and the
/// volume UUID (`docs/src/filesystem/rustfs-spec.md` §8). The payload is
/// sealed and encrypted; only the identity is read here.
fn probe_rustfs(head: &[u8]) -> Option<ProbedVolume> {
    const RUSTFS_SUPERBLOCK_TYPE: u32 = 1;
    if le_u64(head, 0)? != RUSTFS_HEADER_MAGIC {
        return None;
    }
    if le_u32(head, 8)? != RUSTFS_SUPERBLOCK_TYPE {
        return None;
    }
    let mut identity = [0u8; 16];
    identity.copy_from_slice(head.get(16..32)?);
    // A nil UUID never identifies a real volume; refuse the match rather
    // than probe on into a lookalike.
    if identity == [0u8; 16] {
        return None;
    }
    Some(ProbedVolume {
        fstype: VolumeFsType::RustFs,
        identity,
        label: [0u8; LABEL_MAX],
        label_len: 0,
    })
}

/// ext4 (and its ext2/ext3 ancestors): the superblock occupies bytes
/// 1024..2048 with `s_magic` at superblock offset `0x38`, `s_uuid` at
/// `0x68`, and `s_volume_name` at `0x78` (fixed by the ext on-disk
/// format). The magic alone is two bytes, so the structural fields around
/// it are sanity-checked before the match is accepted.
fn probe_ext4(head: &[u8]) -> Option<ProbedVolume> {
    const SUPERBLOCK_OFFSET: usize = 1024;
    let sb = head.get(SUPERBLOCK_OFFSET..SUPERBLOCK_OFFSET + 1024)?;
    if le_u16(sb, 0x38)? != EXT4_SUPERBLOCK_MAGIC {
        return None;
    }
    // s_inodes_count and s_blocks_count_lo are never zero on a real
    // volume, and s_log_block_size beyond 6 (64 KiB blocks) is outside
    // the format.
    if le_u32(sb, 0x00)? == 0 || le_u32(sb, 0x04)? == 0 || le_u32(sb, 0x18)? > 6 {
        return None;
    }
    let mut identity = [0u8; 16];
    identity.copy_from_slice(sb.get(0x68..0x78)?);
    let (label, label_len) = take_label(sb.get(0x78..0x88)?);
    Some(ProbedVolume {
        fstype: VolumeFsType::Ext4,
        identity,
        label,
        label_len,
    })
}

/// FAT32: the boot sector ends in `0x55AA`, carries the `"FAT32   "`
/// extended-BPB tag at byte 82, and its geometry fields must be
/// structurally sane (fixed by the FAT32 on-disk format) — the tag alone
/// is advisory, so the BPB is validated before the match is accepted.
fn probe_fat32(head: &[u8]) -> Option<ProbedVolume> {
    let boot: &[u8; 512] = head.get(..512)?.try_into().ok()?;
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return None;
    }
    if &boot[82..90] != b"FAT32   " {
        return None;
    }
    let bytes_per_sector = le_u16(boot, 11)?;
    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return None;
    }
    let sectors_per_cluster = boot[13];
    if !sectors_per_cluster.is_power_of_two() {
        return None;
    }
    let reserved_sectors = le_u16(boot, 14)?;
    let fat_count = boot[16];
    let fat_sectors = le_u32(boot, 36)?;
    let root_cluster = le_u32(boot, 44)?;
    if reserved_sectors == 0 || !(1..=2).contains(&fat_count) || fat_sectors == 0 {
        return None;
    }
    if root_cluster < 2 {
        return None;
    }
    let identity = fat32_identity_from_boot(boot);
    // The label field exists only under the 0x29 extended boot signature.
    let (label, label_len) = if boot[66] == 0x29 {
        take_label(&boot[71..82])
    } else {
        ([0u8; LABEL_MAX], 0)
    };
    Some(ProbedVolume {
        fstype: VolumeFsType::Fat32,
        identity,
        label,
        label_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structurally valid FAT32 boot sector with the given label bytes
    /// (11, space-padded) and serial.
    fn fat32_boot(serial: [u8; 4], label11: &[u8; 11]) -> [u8; PROBE_HEAD_LEN] {
        let mut head = [0u8; PROBE_HEAD_LEN];
        head[11..13].copy_from_slice(&512u16.to_le_bytes());
        head[13] = 8; // sectors per cluster
        head[14..16].copy_from_slice(&32u16.to_le_bytes());
        head[16] = 2; // FAT count
        head[36..40].copy_from_slice(&1024u32.to_le_bytes());
        head[44..48].copy_from_slice(&2u32.to_le_bytes());
        head[66] = 0x29;
        head[67..71].copy_from_slice(&serial);
        head[71..82].copy_from_slice(label11);
        head[82..90].copy_from_slice(b"FAT32   ");
        head[510] = 0x55;
        head[511] = 0xAA;
        head
    }

    /// A structurally valid ext4 superblock head with the given UUID and
    /// label.
    fn ext4_head(uuid: [u8; 16], label: &[u8]) -> [u8; PROBE_HEAD_LEN] {
        let mut head = [0u8; PROBE_HEAD_LEN];
        let sb = 1024;
        head[sb..sb + 4].copy_from_slice(&8192u32.to_le_bytes()); // inodes
        head[sb + 4..sb + 8].copy_from_slice(&32768u32.to_le_bytes()); // blocks
        head[sb + 0x18..sb + 0x1C].copy_from_slice(&2u32.to_le_bytes()); // 4 KiB
        head[sb + 0x38..sb + 0x3A].copy_from_slice(&EXT4_SUPERBLOCK_MAGIC.to_le_bytes());
        head[sb + 0x68..sb + 0x78].copy_from_slice(&uuid);
        head[sb + 0x78..sb + 0x78 + label.len()].copy_from_slice(label);
        head
    }

    /// A `RustFS` superblock-slot head with the given volume UUID.
    fn rustfs_head(uuid: [u8; 16]) -> [u8; PROBE_HEAD_LEN] {
        let mut head = [0u8; PROBE_HEAD_LEN];
        head[..8].copy_from_slice(&RUSTFS_HEADER_MAGIC.to_le_bytes());
        head[8..12].copy_from_slice(&1u32.to_le_bytes());
        head[16..32].copy_from_slice(&uuid);
        head
    }

    #[test]
    fn recognises_fat32_with_identity_and_label() {
        let head = fat32_boot(*b"SRLN", b"MYDISK     ");
        let probed = probe(&head).expect("matches");
        assert_eq!(probed.fstype, VolumeFsType::Fat32);
        assert_eq!(probed.label(), b"MYDISK");
        let mut expected = [0u8; 16];
        expected[..4].copy_from_slice(b"SRLN");
        expected[4..15].copy_from_slice(b"MYDISK     ");
        expected[15] = 0xF3;
        assert_eq!(probed.identity, expected);
    }

    #[test]
    fn fat32_without_extended_signature_has_tag_only_identity_and_no_label() {
        let mut head = fat32_boot([0; 4], b"           ");
        head[66] = 0; // no extended boot signature
        let probed = probe(&head).expect("matches");
        assert_eq!(probed.label(), b"");
        let mut expected = [0u8; 16];
        expected[15] = 0xF3;
        assert_eq!(probed.identity, expected);
    }

    #[test]
    fn fat32_structural_lies_are_refused() {
        for corrupt in [
            &mut |h: &mut [u8]| h[510] = 0,                          // no 0x55AA
            &mut |h: &mut [u8]| h[82..87].copy_from_slice(b"FAT16"), // wrong tag
            &mut |h: &mut [u8]| h[11..13].copy_from_slice(&513u16.to_le_bytes()),
            &mut |h: &mut [u8]| h[13] = 3, // not a power of two
            &mut |h: &mut [u8]| h[16] = 3, // FAT count
            &mut |h: &mut [u8]| h[36..40].copy_from_slice(&0u32.to_le_bytes()),
            &mut |h: &mut [u8]| h[44..48].copy_from_slice(&1u32.to_le_bytes()),
        ] as [&mut dyn FnMut(&mut [u8]); 7]
        {
            let mut head = fat32_boot(*b"SRLN", b"MYDISK     ");
            corrupt(&mut head);
            assert_eq!(probe(&head), None);
        }
    }

    #[test]
    fn recognises_ext4_with_uuid_and_label() {
        let uuid = [7u8; 16];
        let head = ext4_head(uuid, b"backup\0\0");
        let probed = probe(&head).expect("matches");
        assert_eq!(probed.fstype, VolumeFsType::Ext4);
        assert_eq!(probed.identity, uuid);
        assert_eq!(probed.label(), b"backup");
    }

    #[test]
    fn ext4_sanity_bounds_are_enforced() {
        let uuid = [7u8; 16];
        for corrupt in [
            &mut |h: &mut [u8]| h[1024..1028].copy_from_slice(&0u32.to_le_bytes()),
            &mut |h: &mut [u8]| h[1028..1032].copy_from_slice(&0u32.to_le_bytes()),
            &mut |h: &mut [u8]| h[1024 + 0x18..1024 + 0x1C].copy_from_slice(&7u32.to_le_bytes()),
            &mut |h: &mut [u8]| h[1024 + 0x38] = 0,
        ] as [&mut dyn FnMut(&mut [u8]); 4]
        {
            let mut head = ext4_head(uuid, b"backup");
            corrupt(&mut head);
            assert_eq!(probe(&head), None);
        }
    }

    #[test]
    fn recognises_rustfs_and_refuses_a_nil_uuid() {
        let uuid = [9u8; 16];
        let probed = probe(&rustfs_head(uuid)).expect("matches");
        assert_eq!(probed.fstype, VolumeFsType::RustFs);
        assert_eq!(probed.identity, uuid);
        assert_eq!(probed.label(), b"");

        assert_eq!(probe(&rustfs_head([0u8; 16])), None);
        let mut wrong_type = rustfs_head(uuid);
        wrong_type[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(probe(&wrong_type), None);
    }

    #[test]
    fn a_short_or_empty_head_matches_nothing() {
        assert_eq!(probe(&[]), None);
        assert_eq!(probe(&[0u8; 511]), None);
        let head = fat32_boot(*b"SRLN", b"MYDISK     ");
        // The FAT32 signature needs the full boot sector.
        assert_eq!(probe(&head[..510]), None);
    }

    #[test]
    fn probe_order_is_rustfs_before_ext4_before_fat32() {
        // A head that carries every signature at once resolves to the
        // most-specific match, deterministically.
        let mut head = fat32_boot(*b"SRLN", b"MYDISK     ");
        let ext = ext4_head([7u8; 16], b"backup");
        head[1024..2048].copy_from_slice(&ext[1024..2048]);
        assert_eq!(probe(&head).map(|p| p.fstype), Some(VolumeFsType::Ext4));

        let rust = rustfs_head([9u8; 16]);
        head[..32].copy_from_slice(&rust[..32]);
        assert_eq!(probe(&head).map(|p| p.fstype), Some(VolumeFsType::RustFs));
    }

    #[test]
    fn evidence_windows_cover_each_formats_mutation_surface() {
        // FAT32: the boot sector must name its FSInfo sector inside the
        // reserved region for the window to exist.
        let mut fat = fat32_boot(*b"SRLN", b"MYDISK     ");
        fat[48..50].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(evidence_len(&fat), Some(2 * 512));

        // ext4: the primary superblock ends at byte 2048.
        assert_eq!(evidence_len(&ext4_head([7u8; 16], b"backup")), Some(2048));

        // RustFS: the whole mirrored superblock ring, sized by the
        // plaintext block-size field.
        let mut rust = rustfs_head([9u8; 16]);
        rust[128..132].copy_from_slice(&512u32.to_le_bytes());
        assert_eq!(evidence_len(&rust), Some(RUSTFS_RING_BLOCKS * 512));
        rust[128..132].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(evidence_len(&rust), Some(RUSTFS_RING_BLOCKS * 4096));
    }

    #[test]
    fn lying_evidence_declarations_fail_closed() {
        // An unrecognised head declares nothing.
        assert_eq!(evidence_len(&[0u8; PROBE_HEAD_LEN]), None);

        // FAT32: an absent, zero, or out-of-reserved FSInfo pointer.
        let fat = fat32_boot(*b"SRLN", b"MYDISK     ");
        assert_eq!(evidence_len(&fat), None, "no FSInfo pointer");
        let mut lying = fat;
        lying[48..50].copy_from_slice(&32u16.to_le_bytes()); // == reserved
        assert_eq!(evidence_len(&lying), None);

        // RustFS: a block size outside the format's bounds.
        for bad in [0u32, 256, 768, 8192] {
            let mut rust = rustfs_head([9u8; 16]);
            rust[128..132].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(evidence_len(&rust), None);
        }
    }

    #[test]
    fn fingerprint_is_deterministic_lowercase_and_distinct() {
        let a = fingerprint(&[0u8; 16]);
        assert_eq!(&a, b"00000000000000000000000000");
        let b = fingerprint(&[0xFF; 16]);
        assert!(b
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(a, b);
        // Stable across calls.
        assert_eq!(fingerprint(&[0xFF; 16]), b);
        // The confusable letters never appear.
        for c in *b"ilou" {
            assert!(!b.contains(&c));
        }
    }

    #[test]
    fn fingerprint_encodes_every_identity_bit() {
        // Two identities differing only in the last bit differ in the
        // final character.
        let mut x = [0u8; 16];
        let mut y = [0u8; 16];
        x[15] = 0;
        y[15] = 1;
        assert_ne!(fingerprint(&x), fingerprint(&y));
    }

    #[test]
    fn labels_trim_padding_but_keep_interior_bytes() {
        let (label, len) = take_label(b"A B\0\0");
        assert_eq!(&label[..usize::from(len)], b"A B");
        let (_, len) = take_label(b"     ");
        assert_eq!(len, 0);
    }
}
