//! Host-side backing-disk preparation for storage QEMU integration tests.
//!
//! A virtio-blk integration test needs a backing image whose contents
//! are known *before* the guest boots so the kernel-side test can read a
//! planted sector and assert on it. This module owns the job of laying
//! down a raw block image with a chosen pattern in chosen sectors.
//!
//! Raw images (not qcow2) are used deliberately: QEMU attaches them with
//! `format=raw`, the on-disk byte offset of a logical block is exactly
//! `lba * SECTOR_BYTES`, and the host test harness can re-read the file
//! after the run without linking a qcow2 parser. Keeping the format
//! trivial is what lets the planting and verification logic live in one
//! small, auditable place.

use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// Logical block (sector) size, in bytes, of every backing image this
/// module produces.
///
/// 512 is the size virtio-blk reports by default under QEMU and the unit
/// the kernel-side virtio-blk driver addresses in. The planting API
/// below addresses storage in these units so callers never compute raw
/// byte offsets themselves.
pub const SECTOR_BYTES: usize = 512;

/// Create a zero-filled raw disk image of `size_sectors` sectors at
/// `path`, then write each `(lba, bytes)` entry in `sectors` at its
/// logical block address.
///
/// The file is truncated to exactly `size_sectors * SECTOR_BYTES` bytes,
/// so every block not named in `sectors` reads back as zeroes — a
/// deterministic, reproducible starting state for a storage test
/// (no flaky tests).
///
/// Each planted slice may be shorter than [`SECTOR_BYTES`] (the tail of
/// the sector stays zero) but never longer; an over-long slice is a
/// caller bug and is refused rather than silently spilling into the next
/// block.
///
/// # Errors
///
/// * [`io::ErrorKind::InvalidInput`] — `size_sectors` is `0`, a planted
///   `lba` is `>= size_sectors`, or a planted slice exceeds
///   [`SECTOR_BYTES`]. The image file is not created in these cases.
/// * Other [`io::Error`]s — propagated from creating the parent
///   directory, the file, or the writes.
pub fn plant_raw_disk(path: &Path, size_sectors: u64, sectors: &[(u64, &[u8])]) -> io::Result<()> {
    if size_sectors == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "size_sectors must be non-zero",
        ));
    }
    for (lba, bytes) in sectors {
        if *lba >= size_sectors {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("planted lba {lba} is outside the {size_sectors}-sector image"),
            ));
        }
        if bytes.len() > SECTOR_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "planted sector {lba} is {} bytes, exceeds the {SECTOR_BYTES}-byte block",
                    bytes.len()
                ),
            ));
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.set_len(size_sectors * SECTOR_BYTES as u64)?;

    for (lba, bytes) in sectors {
        file.seek(SeekFrom::Start(lba * SECTOR_BYTES as u64))?;
        file.write_all(bytes)?;
    }
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rustos-qemu-disk-{}-{name}.img",
            std::process::id()
        ))
    }

    #[test]
    fn plants_pattern_and_zero_fills_the_remainder() {
        let path = scratch("plant");
        let sector0: Vec<u8> = (0..SECTOR_BYTES)
            .map(|i| u8::try_from(i % 256).expect("i % 256 fits in u8"))
            .collect();
        plant_raw_disk(&path, 4, &[(0, &sector0)]).expect("plant");

        let raw = fs::read(&path).expect("read back");
        assert_eq!(raw.len(), 4 * SECTOR_BYTES);
        assert_eq!(&raw[..SECTOR_BYTES], sector0.as_slice());
        assert!(
            raw[SECTOR_BYTES..].iter().all(|&b| b == 0),
            "blocks past sector 0 must be zero"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn short_slice_leaves_sector_tail_zero() {
        let path = scratch("short");
        plant_raw_disk(&path, 2, &[(1, &[0xAB, 0xCD])]).expect("plant");

        let raw = fs::read(&path).expect("read back");
        assert_eq!(&raw[SECTOR_BYTES..SECTOR_BYTES + 2], &[0xAB, 0xCD]);
        assert!(raw[SECTOR_BYTES + 2..].iter().all(|&b| b == 0));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zero_size_is_invalid_input() {
        let path = scratch("zero");
        let err = plant_raw_disk(&path, 0, &[]).expect_err("zero size rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists(), "no file is created when validation fails");
    }

    #[test]
    fn out_of_range_lba_is_invalid_input() {
        let path = scratch("oob");
        let err = plant_raw_disk(&path, 2, &[(2, &[0x00])]).expect_err("oob lba rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists());
    }

    #[test]
    fn over_long_slice_is_invalid_input() {
        let path = scratch("long");
        let big = vec![0u8; SECTOR_BYTES + 1];
        let err = plant_raw_disk(&path, 2, &[(0, &big)]).expect_err("over-long slice rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists());
    }
}
