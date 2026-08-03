//! The probe plan: which attachable volumes a block device carries
//! (`plans/DEVICES.md` D3c).
//!
//! The engine reads only device *heads* — never whole volumes — and is
//! pure policy over the [`Block`] seam, so it is host-tested against
//! in-memory images.
//!
//! At each extent (the whole device, then each partition) a **RAID array
//! member** is recognised first ([`tairix_fsprobe::probe_raid_member`]): a
//! member carries a RAID superblock at its block 0, belongs to an array
//! awaiting assembly, and must never be mounted as a bare filesystem — one
//! copy of a mirror mounted read-write diverges the array, and a member that
//! missed writes serves stale data (`AGENTS.md` §26.5). Such an extent is
//! counted ([`PlanSummary::raid_members`]) and skipped, never attached. Only
//! when an extent is *not* a member is it probed for a filesystem, in the
//! fixed and documented order:
//!
//! 1. **Whole-device filesystem first.** A supported signature at LBA 0
//!    (`tairix_fsprobe::probe`) means an unpartitioned "superfloppy"
//!    volume: the device is planned whole and the partition parse is
//!    skipped (a real partition table's LBA 0 never carries a valid
//!    filesystem head, and parsing a boot sector's code bytes as an MBR
//!    would fabricate extents).
//! 2. **Else the partition table** (`lib/partition`: GPT, then MBR,
//!    fail-closed), probing each present partition's head. A partition
//!    whose head matches no supported signature is skipped and counted —
//!    never guessed at. The declared partition *type* is a routing hint
//!    the probe deliberately ignores: the content signature decides.
//!
//! Each recognised volume yields a [`VolumePlan`] carrying its extent,
//! filesystem, stable identity, and the derived **base** catalog name
//! (label-first, else `<fstype><n>` — the [`crate::name`] policy), in
//! stable device order. Collision handling happens at attach time
//! ([`crate::name::candidate`]); the plan itself is one deterministic
//! pass.

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::volume::VolumeFsType;
use tairix_abi::DriverError;
use tairix_fsprobe::{probe, probe_raid_member, ProbedVolume, PROBE_HEAD_LEN};
use tairix_partition::{parse_partition_table, Partition, PartitionError};

use crate::name::{fallback_name, sanitise_label, VolumeName};

/// Largest logical block the head buffer covers (matches the block-size
/// bound the blkio client enforces at connect). It also covers the whole
/// probe head, so one buffer serves both roles.
const HEAD_BUF_LEN: usize = 4096;
const _: () = assert!(HEAD_BUF_LEN >= PROBE_HEAD_LEN);

/// One attachable volume the probe recognised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumePlan {
    /// First logical block of the volume's extent on the device.
    pub first_lba: u64,
    /// Extent length in logical blocks (never zero).
    pub blocks: u64,
    /// The filesystem the signature identified.
    pub fstype: VolumeFsType,
    /// The volume's stable identity, as the filesystem driver publishes it.
    pub identity: [u8; 16],
    /// The derived base catalog name (collision suffixes are applied at
    /// attach time).
    pub base: VolumeName,
}

/// What the device-wide probe pass found.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanSummary {
    /// Volumes recognised and handed to the sink.
    pub planned: u32,
    /// Present partitions whose head matched no supported filesystem.
    pub unrecognised: u32,
    /// Extents (the whole device, or a partition) recognised as a **RAID
    /// array member** and deliberately *not* attached: a member belongs to a
    /// RAID array awaiting assembly, and mounting one bare copy read-write
    /// would diverge a mirror's copies or serve stale data (`AGENTS.md`
    /// §26.5). Not an error, and never counted as blank/unrecognised.
    pub raid_members: u32,
    /// `true` when the device carried no partition table and no
    /// whole-device filesystem (nothing attachable, not an error).
    pub no_scheme: bool,
}

/// Probe `dev` and hand each recognised volume to `sink`, in stable
/// device order.
///
/// # Errors
///
/// [`DriverError`] only for a *device* fault (a failed read); an
/// unrecognised or absent layout is a normal [`PlanSummary`] outcome,
/// never an error.
pub fn plan_volumes<B: Block>(
    dev: &mut B,
    mut sink: impl FnMut(&VolumePlan),
) -> Result<PlanSummary, DriverError> {
    let geometry = dev.geometry()?;
    let mut summary = PlanSummary::default();
    // Per-fstype ordinals for the `<fstype><n>` fallback names, counted
    // in device order so the derivation is stable per layout.
    let mut ordinals = [0u32; 3];

    // Whole-device: a RAID array member first, then a whole-device
    // filesystem (see the module docs for the order and why).
    if let Some((head, len)) = read_head(dev, &geometry, 0, geometry.block_count)? {
        let head = &head[..len];
        if probe_raid_member(head).is_some() {
            // A member occupies the whole device: it belongs to a RAID array
            // awaiting assembly, so it is not attachable as a standalone
            // volume and is emphatically not blank. Fail closed rather than
            // ever mounting one bare copy.
            summary.raid_members = 1;
            return Ok(summary);
        }
        if let Some(probed) = probe(head) {
            sink(&plan_from(&probed, 0, geometry.block_count, &mut ordinals));
            summary.planned = 1;
            return Ok(summary);
        }
    }

    let table = match parse_partition_table(dev) {
        Ok(table) => table,
        Err(PartitionError::Device(err)) => return Err(err),
        // No scheme, or a malformed table: nothing attachable. The device
        // may be blank or foreign; refusing whole is the honest outcome.
        Err(_) => {
            summary.no_scheme = true;
            return Ok(summary);
        }
    };
    for partition in table.partitions() {
        let Some(extent) = bounded_extent(partition, &geometry) else {
            // An extent the geometry cannot hold is a lying table entry;
            // skip it fail-closed rather than read out of range.
            summary.unrecognised += 1;
            continue;
        };
        match read_head(dev, &geometry, extent.0, extent.1)? {
            Some((head, len)) => {
                let head = &head[..len];
                if probe_raid_member(head).is_some() {
                    // A partition that is a RAID array member is skipped for
                    // the same reason as a whole-device member above.
                    summary.raid_members += 1;
                } else if let Some(probed) = probe(head) {
                    sink(&plan_from(&probed, extent.0, extent.1, &mut ordinals));
                    summary.planned += 1;
                } else {
                    summary.unrecognised += 1;
                }
            }
            None => summary.unrecognised += 1,
        }
    }
    Ok(summary)
}

/// Clamp a partition's declared extent against the live geometry, or
/// `None` when it lies outside the device.
fn bounded_extent(partition: &Partition, geometry: &BlockGeometry) -> Option<(u64, u64)> {
    if partition.block_count == 0 {
        return None;
    }
    let end = partition.start_lba.checked_add(partition.block_count)?;
    if end > geometry.block_count {
        return None;
    }
    Some((partition.start_lba, partition.block_count))
}

/// Read an extent's head into a buffer, returning it with the valid byte
/// length, or `None` when the geometry cannot be used to read a head (a zero
/// or oversized block size, or a zero-length extent).
///
/// One read serves both classifiers: the caller passes the returned slice to
/// [`probe_raid_member`] (is this a RAID array member?) and, failing that, to
/// [`probe`] (is this a supported filesystem?), so a device head is read once
/// (`AGENTS.md` §2.16).
fn read_head<B: Block>(
    dev: &mut B,
    geometry: &BlockGeometry,
    first_lba: u64,
    blocks: u64,
) -> Result<Option<([u8; HEAD_BUF_LEN], usize)>, DriverError> {
    let block_size = geometry.block_size as usize;
    if block_size == 0 || block_size > HEAD_BUF_LEN || blocks == 0 {
        return Ok(None);
    }
    // Whole blocks covering the probe head, bounded by the extent. The
    // arithmetic stays in `usize`: `wanted` is small by construction, and
    // an extent too large for `usize` cannot bound it further.
    let wanted = PROBE_HEAD_LEN.div_ceil(block_size);
    let read_blocks = usize::try_from(blocks).map_or(wanted, |extent| wanted.min(extent));
    let read_bytes = read_blocks * block_size;
    let mut head = [0u8; HEAD_BUF_LEN];
    let take = read_bytes.min(HEAD_BUF_LEN);
    dev.read_blocks(first_lba, &mut head[..take])?;
    Ok(Some((head, take)))
}

/// Build the plan record for one recognised volume, deriving its base
/// name and advancing its type's fallback ordinal.
fn plan_from(
    probed: &ProbedVolume,
    first_lba: u64,
    blocks: u64,
    ordinals: &mut [u32; 3],
) -> VolumePlan {
    let slot = match probed.fstype {
        VolumeFsType::ARXFS => 0,
        VolumeFsType::Ext4 => 1,
        VolumeFsType::Fat32 => 2,
    };
    ordinals[slot] += 1;
    let base = sanitise_label(probed.label())
        .unwrap_or_else(|| fallback_name(probed.fstype, ordinals[slot]));
    VolumePlan {
        first_lba,
        blocks,
        fstype: probed.fstype,
        identity: probed.identity,
        base,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_partition::{mbr, PartitionType};

    use super::*;

    const BS: usize = 512;
    const BS_U32: u32 = 512;

    /// An in-memory block device over a byte image.
    struct MemBlock {
        image: Vec<u8>,
        faulty: bool,
    }

    impl Block for MemBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BS_U32,
                block_count: (self.image.len() / BS) as u64,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            if self.faulty {
                return Err(DriverError::DeviceFault);
            }
            let start = usize::try_from(lba).map_err(|_| DriverError::OutOfRange)? * BS;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::OutOfRange)?;
            let src = self.image.get(start..end).ok_or(DriverError::OutOfRange)?;
            buf.copy_from_slice(src);
            Ok(())
        }

        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// A structurally valid FAT32 head written at byte `offset` of `image`.
    fn write_fat32(image: &mut [u8], offset: usize, serial: [u8; 4], label11: &[u8; 11]) {
        let boot = &mut image[offset..offset + 512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 8;
        boot[14..16].copy_from_slice(&32u16.to_le_bytes());
        boot[16] = 2;
        boot[36..40].copy_from_slice(&1024u32.to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[66] = 0x29;
        boot[67..71].copy_from_slice(&serial);
        boot[71..82].copy_from_slice(label11);
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510] = 0x55;
        boot[511] = 0xAA;
    }

    /// A structurally valid ext4 superblock written at partition byte
    /// `offset` of `image` (the superblock sits 1024 bytes in).
    fn write_ext4(image: &mut [u8], offset: usize, uuid: [u8; 16], label: &[u8]) {
        let sb = &mut image[offset + 1024..offset + 2048];
        sb[0..4].copy_from_slice(&8192u32.to_le_bytes());
        sb[4..8].copy_from_slice(&32768u32.to_le_bytes());
        sb[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes());
        sb[0x38..0x3A].copy_from_slice(&tairix_fsprobe::EXT4_SUPERBLOCK_MAGIC.to_le_bytes());
        sb[0x68..0x78].copy_from_slice(&uuid);
        sb[0x78..0x78 + label.len()].copy_from_slice(label);
    }

    /// Rewrite entry `index`'s MBR type byte, so a test can author the
    /// foreign (e.g. Linux `0x83`) partitions `mbr::encode` deliberately
    /// refuses to spell.
    fn set_type_byte(sector: &mut [u8; 512], index: usize, byte: u8) {
        sector[446 + index * 16 + 4] = byte;
    }

    fn collect(dev: &mut MemBlock) -> (Vec<VolumePlan>, PlanSummary) {
        let mut plans = Vec::new();
        let summary = plan_volumes(dev, |plan| plans.push(*plan)).expect("plans");
        (plans, summary)
    }

    #[test]
    fn a_superfloppy_is_planned_whole() {
        let mut image = vec![0u8; 64 * BS];
        write_fat32(&mut image, 0, *b"SRLN", b"HOLIDAY PIX");
        let mut dev = MemBlock {
            image,
            faulty: false,
        };
        let (plans, summary) = collect(&mut dev);
        assert_eq!(summary.planned, 1);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].first_lba, 0);
        assert_eq!(plans[0].blocks, 64);
        assert_eq!(plans[0].fstype, VolumeFsType::Fat32);
        assert_eq!(plans[0].base.as_bytes(), b"holidaypix");
    }

    #[test]
    fn partitions_are_probed_by_content_with_fallback_names() {
        // MBR: partition 1 = FAT32 with a blank label (fallback name),
        // partition 2 = ext4 with a label, partition 3 = unrecognised.
        let mut image = vec![0u8; 256 * BS];
        let parts = [
            Partition {
                ty: PartitionType::FatBoot,
                start_lba: 8,
                block_count: 64,
            },
            Partition {
                ty: PartitionType::FatBoot,
                start_lba: 80,
                block_count: 64,
            },
            Partition {
                ty: PartitionType::FatBoot,
                start_lba: 160,
                block_count: 32,
            },
        ];
        let mut sector = mbr::encode(&parts).expect("encodes");
        // Partitions 2 and 3 are foreign (Linux) — the declared type is a
        // hint the probe ignores; the content decides.
        set_type_byte(&mut sector, 1, 0x83);
        set_type_byte(&mut sector, 2, 0x83);
        image[..512].copy_from_slice(&sector);
        write_fat32(&mut image, 8 * BS, [1, 2, 3, 4], b"           ");
        write_ext4(&mut image, 80 * BS, [7u8; 16], b"backup");
        let mut dev = MemBlock {
            image,
            faulty: false,
        };

        let (plans, summary) = collect(&mut dev);
        assert_eq!(summary.planned, 2);
        assert_eq!(summary.unrecognised, 1);
        assert!(!summary.no_scheme);

        assert_eq!(plans[0].first_lba, 8);
        assert_eq!(plans[0].fstype, VolumeFsType::Fat32);
        assert_eq!(
            plans[0].base.as_bytes(),
            b"fat1",
            "a blank label falls back to <fstype><n>"
        );
        assert_eq!(plans[1].first_lba, 80);
        assert_eq!(plans[1].fstype, VolumeFsType::Ext4);
        assert_eq!(plans[1].base.as_bytes(), b"backup");
        assert_eq!(plans[1].identity, [7u8; 16]);
    }

    #[test]
    fn a_lying_extent_is_skipped_without_reading_past_the_device() {
        // The table declares a partition reaching past the device end.
        let mut image = vec![0u8; 64 * BS];
        let parts = [Partition {
            ty: PartitionType::FatBoot,
            start_lba: 32,
            block_count: 1024,
        }];
        let mut sector = mbr::encode(&parts).expect("encodes");
        set_type_byte(&mut sector, 0, 0x83);
        image[..512].copy_from_slice(&sector);
        let mut dev = MemBlock {
            image,
            faulty: false,
        };
        let (plans, summary) = collect(&mut dev);
        assert!(plans.is_empty());
        assert_eq!(summary.unrecognised, 1);
    }

    #[test]
    fn a_blank_device_plans_nothing() {
        let mut dev = MemBlock {
            image: vec![0u8; 64 * BS],
            faulty: false,
        };
        let (plans, summary) = collect(&mut dev);
        assert!(plans.is_empty());
        assert_eq!(summary.planned, 0);
        assert!(summary.no_scheme);
    }

    #[test]
    fn a_device_fault_is_an_error_not_an_empty_plan() {
        let mut dev = MemBlock {
            image: vec![0u8; 64 * BS],
            faulty: true,
        };
        assert_eq!(
            plan_volumes(&mut dev, |_| {}),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn two_unlabelled_volumes_of_one_type_get_distinct_ordinals() {
        let mut image = vec![0u8; 256 * BS];
        let parts = [
            Partition {
                ty: PartitionType::FatBoot,
                start_lba: 8,
                block_count: 64,
            },
            Partition {
                ty: PartitionType::FatBoot,
                start_lba: 80,
                block_count: 64,
            },
        ];
        let mut sector = mbr::encode(&parts).expect("encodes");
        set_type_byte(&mut sector, 0, 0x83);
        set_type_byte(&mut sector, 1, 0x83);
        image[..512].copy_from_slice(&sector);
        write_fat32(&mut image, 8 * BS, [1, 1, 1, 1], b"           ");
        write_fat32(&mut image, 80 * BS, [2, 2, 2, 2], b"           ");
        let mut dev = MemBlock {
            image,
            faulty: false,
        };
        let (plans, _) = collect(&mut dev);
        assert_eq!(plans[0].base.as_bytes(), b"fat1");
        assert_eq!(plans[1].base.as_bytes(), b"fat2");
    }

    /// Write a valid RAID array-member superblock (a RAID1 mirror, member 0 of
    /// 2) into the first bytes of the extent at byte `offset`, exactly as a
    /// member carries it at its block 0.
    fn write_raid_member(image: &mut [u8], offset: usize, uuid: [u8; 16]) {
        let superblock = tairix_raidmeta::ArraySuperblock {
            array_uuid: uuid,
            raid_level: tairix_abi::raid::RaidLevel::Mirror,
            member_count: 2,
            member_slot: 0,
            geometry: BlockGeometry {
                block_size: BS_U32,
                block_count: 64,
            },
            generation: 5,
            updated_at: tairix_abi::time::Time64::from_secs(1_700_000_000),
            chunk_blocks: 0,
        };
        let encoded = superblock.encode();
        image[offset..offset + encoded.len()].copy_from_slice(&encoded);
    }

    #[test]
    fn a_whole_device_raid_member_is_recognised_and_never_attached() {
        // The device's block 0 is a RAID member superblock, not a partition
        // table or a filesystem. It must be recognised and skipped — mounting
        // one bare copy of an array would diverge the array or serve stale
        // data — never mistaken for a blank device.
        let mut image = vec![0u8; 64 * BS];
        write_raid_member(&mut image, 0, [0x5A; 16]);
        let mut dev = MemBlock {
            image,
            faulty: false,
        };
        let (plans, summary) = collect(&mut dev);
        assert!(plans.is_empty(), "a bare RAID member is never attached");
        assert_eq!(summary.raid_members, 1);
        assert_eq!(summary.planned, 0);
        assert_eq!(summary.unrecognised, 0);
        assert!(!summary.no_scheme, "a member is not a blank device");
    }

    #[test]
    fn a_partition_that_is_a_raid_member_is_recognised_and_never_attached() {
        // A partitioned disk whose one partition is a RAID member: the table
        // parses, but the member partition is skipped rather than attached.
        let mut image = vec![0u8; 64 * BS];
        let parts = [Partition {
            ty: PartitionType::FatBoot,
            start_lba: 8,
            block_count: 32,
        }];
        let mut sector = mbr::encode(&parts).expect("encodes");
        set_type_byte(&mut sector, 0, 0x83);
        image[..512].copy_from_slice(&sector);
        write_raid_member(&mut image, 8 * BS, [0xC3; 16]);
        let mut dev = MemBlock {
            image,
            faulty: false,
        };
        let (plans, summary) = collect(&mut dev);
        assert!(plans.is_empty());
        assert_eq!(summary.raid_members, 1);
        assert_eq!(summary.planned, 0);
        assert_eq!(summary.unrecognised, 0);
    }
}
