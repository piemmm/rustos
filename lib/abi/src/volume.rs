//! Runtime volume attach/detach request frames (`plans/DEVICES.md` D3b).
//!
//! A volume manager that has probed a hot-pluggable block source — a
//! [`crate::blkio`] block-service endpoint plus its shared data window,
//! both inherited as grants from the storage node the block driver emitted
//! — asks the kernel to **attach** a filesystem driver to it with a
//! [`VolumeAttachRequest`] (the `volume_attach` syscall) and to take it
//! back out of service with a [`VolumeDetachRequest`] (`volume_detach`).
//! Both operations require `CAP_FS_MOUNT` and are audited; every field is
//! validated fail-closed here *and* re-checked kernel-side against live
//! state (the endpoint, the window, the device geometry) before anything
//! is mounted.
//!
//! The frames are fixed-shape and bounded: the attach request carries the
//! partition extent the caller probed (`lib/partition`), the filesystem
//! type its signature probe matched, and the catalog name the volume's
//! root is projected under (`/Storage/<name>` — the `Storage:` catalog
//! view location). Naming *policy* (label sanitisation, collision
//! fingerprints) is the volume manager's job; this layer enforces only
//! the structural spelling bounds so a malformed name can never reach the
//! mount table.

use crate::le::{put_u64, read_u64};
use crate::Errno;

/// The filesystem drivers a volume can be attached with. Closed set: an
/// unknown discriminant is refused at decode (fail closed), and a new
/// filesystem extends this enum together with the kernel service that
/// opens it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum VolumeFsType {
    /// The native filesystem (`drivers/filesystem/rustfs`).
    RustFs = 0,
    /// ext2/ext3/ext4 (`drivers/filesystem/ext4`).
    Ext4 = 1,
    /// FAT32 (`drivers/filesystem/fat32`).
    Fat32 = 2,
}

impl VolumeFsType {
    /// The wire byte for this filesystem type.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a filesystem type from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no known filesystem type.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::RustFs),
            1 => Ok(Self::Ext4),
            2 => Ok(Self::Fat32),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Maximum byte length of a volume's catalog name. A fixed security bound
/// on untrusted input, not a growable capacity: catalog names are short
/// human labels, and the mount-table spelling below `/Storage/` must stay
/// bounded.
pub const VOLUME_NAME_MAX: usize = 32;

/// Byte length of the 16-byte volume identity every detach names — the
/// same stable identity the volume forest publishes for `id::` paths.
pub const VOLUME_ID_LEN: usize = 16;

/// Fixed prefix of a [`VolumeAttachRequest`] frame:
/// `endpoint(8) || window(8) || first_lba(8) || blocks(8) || fstype(1) ||
/// name_len(1)`, followed by exactly `name_len` name bytes.
const ATTACH_FIXED_LEN: usize = 8 + 8 + 8 + 8 + 1 + 1;

/// Maximum encoded length of a [`VolumeAttachRequest`].
pub const VOLUME_ATTACH_MAX_LEN: usize = ATTACH_FIXED_LEN + VOLUME_NAME_MAX;

/// Encoded length of a [`VolumeDetachRequest`]: the bare volume identity.
pub const VOLUME_DETACH_LEN: usize = VOLUME_ID_LEN;

/// `true` if `byte` may appear in a volume catalog name.
///
/// The structural subset of the alias character rules: ASCII letters,
/// digits, `-`, and `_`. Anything else — separators, dots, control bytes,
/// non-ASCII — is refused, so a name can never spell a path component
/// like `..`, an empty segment, or a terminal escape.
const fn name_byte_ok(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Validate a volume catalog name's structural spelling.
///
/// # Errors
///
/// [`Errno::OutOfRange`] when the name is empty, longer than
/// [`VOLUME_NAME_MAX`], or contains a byte outside the permitted set.
pub fn validate_volume_name(name: &[u8]) -> Result<(), Errno> {
    if name.is_empty() || name.len() > VOLUME_NAME_MAX {
        return Err(Errno::OutOfRange);
    }
    let mut i = 0;
    while i < name.len() {
        if !name_byte_ok(name[i]) {
            return Err(Errno::OutOfRange);
        }
        i += 1;
    }
    Ok(())
}

/// One runtime volume-attach request: attach `fstype` to the block extent
/// `[first_lba, first_lba + blocks)` served by the block-service
/// `endpoint` + shared data `window`, and publish its root under the
/// catalog name `name`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeAttachRequest<'a> {
    /// The block-service call endpoint id (the storage node's endpoint
    /// grant).
    pub endpoint: u64,
    /// The shared data window's region id (the storage node's
    /// shared-memory grant).
    pub window: u64,
    /// First logical block of the partition extent the filesystem lives
    /// in. Zero for a whole-device filesystem.
    pub first_lba: u64,
    /// Length of the extent in logical blocks. Must be non-zero; the
    /// kernel re-checks the extent against the device's live geometry.
    pub blocks: u64,
    /// The filesystem driver to attach.
    pub fstype: VolumeFsType,
    /// The catalog name the volume's root is projected under
    /// (`/Storage/<name>`), already policy-derived by the caller and
    /// structurally validated here.
    pub name: &'a [u8],
}

impl<'a> VolumeAttachRequest<'a> {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] for a structurally invalid name or a
    ///   zero-length extent — a frame that cannot mean what it says is
    ///   never produced.
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold the encoding.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        validate_volume_name(self.name)?;
        if self.blocks == 0 {
            return Err(Errno::OutOfRange);
        }
        let len = ATTACH_FIXED_LEN + self.name.len();
        if buf.len() < len {
            return Err(Errno::BufferTooSmall);
        }
        put_u64(buf, 0, self.endpoint);
        put_u64(buf, 8, self.window);
        put_u64(buf, 16, self.first_lba);
        put_u64(buf, 24, self.blocks);
        buf[32] = self.fstype.as_u8();
        // Bounded by `VOLUME_NAME_MAX` (≤ 255), so the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[33] = self.name.len() as u8;
        }
        buf[ATTACH_FIXED_LEN..len].copy_from_slice(self.name);
        Ok(len)
    }

    /// Decode an attach request from `bytes`, validating every field.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] when `bytes` is shorter than the
    ///   fixed prefix, longer than [`VOLUME_ATTACH_MAX_LEN`], or does not
    ///   carry exactly `name_len` name bytes — a frame is exact, never
    ///   padded or truncated.
    /// * [`Errno::OutOfRange`] for an unknown filesystem type, a
    ///   zero-length extent, an extent whose end overflows, or a
    ///   structurally invalid name.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < ATTACH_FIXED_LEN || bytes.len() > VOLUME_ATTACH_MAX_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let endpoint = read_u64(bytes, 0);
        let window = read_u64(bytes, 8);
        let first_lba = read_u64(bytes, 16);
        let blocks = read_u64(bytes, 24);
        let fstype = VolumeFsType::from_u8(bytes[32])?;
        let name_len = usize::from(bytes[33]);
        if bytes.len() != ATTACH_FIXED_LEN + name_len {
            return Err(Errno::LengthOutOfRange);
        }
        if blocks == 0 || first_lba.checked_add(blocks).is_none() {
            return Err(Errno::OutOfRange);
        }
        let name = &bytes[ATTACH_FIXED_LEN..];
        validate_volume_name(name)?;
        Ok(Self {
            endpoint,
            window,
            first_lba,
            blocks,
            fstype,
            name,
        })
    }
}

/// One runtime volume-detach request: take the volume published under
/// `volume_id` out of service (flush, unmount, unpublish its root).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeDetachRequest {
    /// The volume's stable 16-byte identity, as the forest published it.
    pub volume_id: [u8; VOLUME_ID_LEN],
}

impl VolumeDetachRequest {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] for the reserved all-zero identity, which
    ///   is never published and so can never be detached.
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold
    ///   [`VOLUME_DETACH_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.volume_id == [0u8; VOLUME_ID_LEN] {
            return Err(Errno::OutOfRange);
        }
        if buf.len() < VOLUME_DETACH_LEN {
            return Err(Errno::BufferTooSmall);
        }
        buf[..VOLUME_DETACH_LEN].copy_from_slice(&self.volume_id);
        Ok(VOLUME_DETACH_LEN)
    }

    /// Decode a detach request from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is not exactly
    ///   [`VOLUME_DETACH_LEN`] bytes.
    /// * [`Errno::OutOfRange`] for the reserved all-zero identity (fail
    ///   closed; it can never name a published volume).
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != VOLUME_DETACH_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let mut volume_id = [0u8; VOLUME_ID_LEN];
        volume_id.copy_from_slice(bytes);
        if volume_id == [0u8; VOLUME_ID_LEN] {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { volume_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attach() -> VolumeAttachRequest<'static> {
        VolumeAttachRequest {
            endpoint: 0x004D_5344_0000_0001,
            window: 7,
            first_lba: 2048,
            blocks: 1 << 21,
            fstype: VolumeFsType::Fat32,
            name: b"usb1",
        }
    }

    #[test]
    fn fstype_round_trips_and_rejects_unknown() {
        for fstype in [
            VolumeFsType::RustFs,
            VolumeFsType::Ext4,
            VolumeFsType::Fat32,
        ] {
            assert_eq!(VolumeFsType::from_u8(fstype.as_u8()), Ok(fstype));
        }
        assert_eq!(VolumeFsType::from_u8(3), Err(Errno::OutOfRange));
    }

    #[test]
    fn attach_round_trips() {
        let req = attach();
        let mut buf = [0u8; VOLUME_ATTACH_MAX_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, ATTACH_FIXED_LEN + 4);
        assert_eq!(VolumeAttachRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn attach_name_bounds_are_enforced_both_ways() {
        let mut buf = [0u8; VOLUME_ATTACH_MAX_LEN + 8];
        for bad in [
            &b""[..],
            &[b'a'; VOLUME_NAME_MAX + 1][..],
            b"a/b",
            b"a b",
            b"..",
            b"caf\xC3\xA9",
        ] {
            let req = VolumeAttachRequest {
                name: bad,
                ..attach()
            };
            assert_eq!(req.encode(&mut buf), Err(Errno::OutOfRange), "{bad:?}");
        }
        // A maximum-length name is fine.
        let req = VolumeAttachRequest {
            name: &[b'a'; VOLUME_NAME_MAX],
            ..attach()
        };
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, VOLUME_ATTACH_MAX_LEN);
        assert_eq!(VolumeAttachRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn attach_decode_rejects_wrong_lengths() {
        let req = attach();
        let mut buf = [0u8; VOLUME_ATTACH_MAX_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        // Truncated fixed prefix, truncated name, and trailing garbage all
        // fail closed.
        assert_eq!(
            VolumeAttachRequest::decode(&buf[..ATTACH_FIXED_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            VolumeAttachRequest::decode(&buf[..n - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            VolumeAttachRequest::decode(&buf[..=n]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn attach_decode_rejects_bad_fields() {
        let mut buf = [0u8; VOLUME_ATTACH_MAX_LEN];
        let n = attach().encode(&mut buf).expect("encodes");
        // Unknown filesystem type.
        let mut bad = buf;
        bad[32] = 9;
        assert_eq!(
            VolumeAttachRequest::decode(&bad[..n]),
            Err(Errno::OutOfRange)
        );
        // Zero-length extent.
        let mut bad = buf;
        put_u64(&mut bad, 24, 0);
        assert_eq!(
            VolumeAttachRequest::decode(&bad[..n]),
            Err(Errno::OutOfRange)
        );
        // Extent end overflows the block-address space.
        let mut bad = buf;
        put_u64(&mut bad, 16, u64::MAX);
        assert_eq!(
            VolumeAttachRequest::decode(&bad[..n]),
            Err(Errno::OutOfRange)
        );
        // A name byte outside the permitted set.
        let mut bad = buf;
        bad[ATTACH_FIXED_LEN] = b'/';
        assert_eq!(
            VolumeAttachRequest::decode(&bad[..n]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn detach_round_trips_and_rejects_nil_and_wrong_lengths() {
        let req = VolumeDetachRequest { volume_id: [7; 16] };
        let mut buf = [0u8; VOLUME_DETACH_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, VOLUME_DETACH_LEN);
        assert_eq!(VolumeDetachRequest::decode(&buf[..n]), Ok(req));

        assert_eq!(
            VolumeDetachRequest { volume_id: [0; 16] }.encode(&mut buf),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            VolumeDetachRequest::decode(&[0u8; VOLUME_DETACH_LEN]),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            VolumeDetachRequest::decode(&buf[..VOLUME_DETACH_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
    }
}
