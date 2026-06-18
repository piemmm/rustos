//! Unit tests for the scheme-neutral partition model, the MBR and GPT
//! parsers, and the partition-window [`Block`] adapter.

use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::gpt::{self, ENTRY_LEN, TYPE_GUID_EFI_SYSTEM, TYPE_GUID_RUSTFS_ROOT};
use crate::mbr::{self, MbrError, MBR_SECTOR_LEN};
use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::DriverError;

/// An in-memory [`Block`] device over a byte vector, for building and
/// parsing disk images in tests.
struct VecBlock {
    data: Vec<u8>,
    block_size: u32,
}

impl VecBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        let len = block_size as usize * usize::try_from(block_count).expect("fits");
        Self {
            data: vec![0u8; len],
            block_size,
        }
    }

    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = self.block_size as usize;
        if bs == 0 || len == 0 || len % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.data.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }

    fn put(&mut self, lba: u64, bytes: &[u8]) {
        let start = usize::try_from(lba).expect("fits") * self.block_size as usize;
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

impl Block for VecBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: self.data.len() as u64 / u64::from(self.block_size),
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }
}

fn boot(start: u32, sectors: u32) -> Partition {
    Partition {
        ty: PartitionType::FatBoot,
        start_lba: u64::from(start),
        block_count: u64::from(sectors),
    }
}

fn root(start: u32, sectors: u32) -> Partition {
    Partition {
        ty: PartitionType::RustFsRoot,
        start_lba: u64::from(start),
        block_count: u64::from(sectors),
    }
}

fn system(start: u32, sectors: u32) -> Partition {
    Partition {
        ty: PartitionType::RustFsSystem,
        start_lba: u64::from(start),
        block_count: u64::from(sectors),
    }
}

// ----------------------------------------------------------------------
// MBR
// ----------------------------------------------------------------------

#[test]
fn mbr_round_trips_the_boot_and_root_layout() {
    let parts = [boot(2048, 65536), root(67584, 1_000_000)];
    let sector = mbr::encode(&parts).expect("encodes");
    let table = mbr::parse(&sector).expect("parses");

    assert_eq!(table.partitions().len(), 2);
    assert_eq!(table.first_of_type(PartitionType::FatBoot), Some(parts[0]));
    assert_eq!(
        table.first_of_type(PartitionType::RustFsRoot),
        Some(parts[1])
    );
    assert_eq!(table.first_of_type(PartitionType::Other), None);
}

#[test]
fn mbr_round_trips_the_three_partition_design_b_layout() {
    // The design-B image: FAT boot + read-only `/System` + encrypted data
    // root, each located by role and distinct on the wire (`plans/PI.md`).
    let parts = [
        system(67584, 65536),
        boot(2048, 65536),
        root(133_120, 65536),
    ];
    let sector = mbr::encode(&parts).expect("encodes");

    // The `/System` entry carries its own distinct type byte, separate from
    // the encrypted data root's.
    assert_eq!(sector[446 + 4], mbr::PART_TYPE_RUSTFS_SYSTEM);
    assert_ne!(mbr::PART_TYPE_RUSTFS_SYSTEM, mbr::PART_TYPE_RUSTFS);

    let table = mbr::parse(&sector).expect("parses");
    assert_eq!(table.partitions().len(), 3);
    assert_eq!(table.first_of_type(PartitionType::FatBoot), Some(parts[1]));
    assert_eq!(
        table.first_of_type(PartitionType::RustFsSystem),
        Some(parts[0])
    );
    assert_eq!(
        table.first_of_type(PartitionType::RustFsRoot),
        Some(parts[2])
    );
}

#[test]
fn mbr_type_byte_round_trips_every_role() {
    for ty in [
        PartitionType::FatBoot,
        PartitionType::RustFsSystem,
        PartitionType::RustFsRoot,
    ] {
        assert_eq!(mbr::classify(mbr::type_byte_for(ty)), ty);
    }
}

#[test]
fn gpt_classifies_the_system_guid_distinctly_from_the_root_guid() {
    assert_eq!(
        gpt::classify(&gpt::TYPE_GUID_RUSTFS_SYSTEM),
        PartitionType::RustFsSystem
    );
    assert_eq!(
        gpt::classify(&TYPE_GUID_RUSTFS_ROOT),
        PartitionType::RustFsRoot
    );
    assert_ne!(gpt::TYPE_GUID_RUSTFS_SYSTEM, TYPE_GUID_RUSTFS_ROOT);
}

#[test]
fn mbr_encode_rejects_overlap() {
    let parts = [boot(2048, 4096), root(4096, 4096)];
    assert_eq!(mbr::encode(&parts), Err(MbrError::Overlap));
}

#[test]
fn mbr_encode_rejects_empty_and_sector_zero_and_too_many() {
    assert_eq!(mbr::encode(&[]), Err(MbrError::NoPartitions));
    assert_eq!(
        mbr::encode(&[boot(0, 4096)]),
        Err(MbrError::CoversMbrSector)
    );
    assert_eq!(mbr::encode(&[boot(2048, 0)]), Err(MbrError::EmptyPartition));
    let five = [
        boot(2048, 16),
        root(2064, 16),
        boot(2080, 16),
        root(2096, 16),
        boot(2112, 16),
    ];
    assert_eq!(mbr::encode(&five), Err(MbrError::TooManyPartitions));
}

#[test]
fn mbr_encode_rejects_a_partition_too_large_for_32_bit_fields() {
    let huge = Partition {
        ty: PartitionType::RustFsRoot,
        start_lba: u64::from(u32::MAX) + 1,
        block_count: 64,
    };
    assert_eq!(mbr::encode(&[huge]), Err(MbrError::ExtentTooLarge));
}

#[test]
fn mbr_parse_rejects_a_missing_signature() {
    let mut sector = mbr::encode(&[root(2048, 4096)]).unwrap();
    sector[MBR_SECTOR_LEN - 1] = 0x00;
    assert_eq!(mbr::parse(&sector), Err(MbrError::BadSignature));
}

#[test]
fn mbr_parse_rejects_a_short_sector() {
    let short = [0u8; MBR_SECTOR_LEN - 1];
    assert_eq!(mbr::parse(&short), Err(MbrError::ShortSector));
}

#[test]
fn mbr_parse_rejects_overlapping_on_disk_entries() {
    // Hand-author a table the encoder would never produce: two
    // overlapping entries. The parser must reject it whole.
    let mut sector = [0u8; MBR_SECTOR_LEN];
    for (i, (start, sectors)) in [(2048u32, 4096u32), (4096, 4096)].iter().enumerate() {
        let base = mbr::PARTITION_TABLE_OFFSET + i * mbr::PARTITION_ENTRY_LEN;
        sector[base + 4] = mbr::PART_TYPE_RUSTFS;
        sector[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        sector[base + 12..base + 16].copy_from_slice(&sectors.to_le_bytes());
    }
    sector[MBR_SECTOR_LEN - 2] = mbr::MBR_SIGNATURE[0];
    sector[MBR_SECTOR_LEN - 1] = mbr::MBR_SIGNATURE[1];
    assert_eq!(mbr::parse(&sector), Err(MbrError::Overlap));
}

// ----------------------------------------------------------------------
// GPT
// ----------------------------------------------------------------------

/// Build a GPT disk image (protective MBR + primary header + entry array)
/// with the given partitions, computing both CRCs with the crate's own
/// [`gpt::crc32`] so the parser is exercised against a self-consistent
/// image (the standard way to test a parser absent an in-tree encoder).
fn build_gpt(
    block_size: u32,
    block_count: u64,
    num_entries: u32,
    parts: &[([u8; 16], u64, u64)],
) -> VecBlock {
    let bs = block_size as usize;
    let mut dev = VecBlock::new(block_size, block_count);

    // Protective MBR (LBA 0): one 0xEE entry plus the signature.
    let mut pmbr = vec![0u8; bs];
    pmbr[mbr::PARTITION_TABLE_OFFSET + 4] = 0xee;
    pmbr[bs - 2] = mbr::MBR_SIGNATURE[0];
    pmbr[bs - 1] = mbr::MBR_SIGNATURE[1];
    dev.put(0, &pmbr);

    // Entry array (from LBA 2).
    let region_len = num_entries as usize * ENTRY_LEN;
    let mut region = vec![0u8; region_len];
    for (i, (guid, first, last)) in parts.iter().enumerate() {
        let base = i * ENTRY_LEN;
        region[base..base + 16].copy_from_slice(guid);
        // Unique partition GUID (off 16..32): just the index, enough to be
        // non-zero/distinct.
        region[base + 16] = u8::try_from(i).expect("few test parts").wrapping_add(1);
        region[base + 32..base + 40].copy_from_slice(&first.to_le_bytes());
        region[base + 40..base + 48].copy_from_slice(&last.to_le_bytes());
    }
    let entries_crc = gpt::crc32(&region);
    // Write the region across consecutive blocks from LBA 2.
    let entries_lba = 2u64;
    for (i, chunk) in region.chunks(bs).enumerate() {
        dev.put(entries_lba + i as u64, chunk);
    }

    // Primary header (LBA 1).
    let mut hdr = vec![0u8; bs];
    hdr[0..8].copy_from_slice(&gpt::HEADER_SIGNATURE);
    hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    hdr[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
    hdr[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    hdr[80..84].copy_from_slice(&num_entries.to_le_bytes());
    hdr[84..88].copy_from_slice(&u32::try_from(ENTRY_LEN).expect("fits").to_le_bytes());
    hdr[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let header_crc = gpt::crc32(&hdr[..92]);
    hdr[16..20].copy_from_slice(&header_crc.to_le_bytes());
    dev.put(1, &hdr);

    dev
}

#[test]
fn gpt_parses_boot_and_root_partitions() {
    let parts = [
        (TYPE_GUID_EFI_SYSTEM, 2048u64, 67583u64),
        (TYPE_GUID_RUSTFS_ROOT, 67584, 200_000),
    ];
    let mut dev = build_gpt(512, 262_144, 128, &parts);

    let table = parse_partition_table(&mut dev).expect("parses GPT");
    assert_eq!(table.partitions().len(), 2);
    let bootp = table.first_of_type(PartitionType::FatBoot).expect("boot");
    assert_eq!(bootp.start_lba, 2048);
    assert_eq!(bootp.block_count, 67583 - 2048 + 1);
    let rootp = table
        .first_of_type(PartitionType::RustFsRoot)
        .expect("root");
    assert_eq!(rootp.start_lba, 67584);
    assert_eq!(rootp.block_count, 200_000 - 67584 + 1);
}

#[test]
fn gpt_parses_with_a_4k_logical_block() {
    let parts = [(TYPE_GUID_RUSTFS_ROOT, 256u64, 50_000u64)];
    let mut dev = build_gpt(4096, 65_536, 128, &parts);
    let table = parse_partition_table(&mut dev).expect("parses 4k GPT");
    assert_eq!(
        table
            .first_of_type(PartitionType::RustFsRoot)
            .unwrap()
            .start_lba,
        256
    );
}

#[test]
fn gpt_rejects_a_corrupt_header_crc() {
    let parts = [(TYPE_GUID_RUSTFS_ROOT, 2048u64, 4096u64)];
    let mut dev = build_gpt(512, 8192, 128, &parts);
    // Flip a header byte the CRC covers (the entries-LBA field).
    let mut hdr = [0u8; 512];
    dev.read_blocks(1, &mut hdr).unwrap();
    hdr[72] ^= 0xff;
    dev.write_blocks(1, &hdr).unwrap();
    // Detection now fails -> falls back to MBR (the protective MBR has no
    // RustFS entry), so no scheme yields a usable table.
    assert!(matches!(
        parse_partition_table(&mut dev),
        Err(PartitionError::Mbr(_))
    ));
}

#[test]
fn gpt_rejects_a_corrupt_entries_crc() {
    let parts = [(TYPE_GUID_RUSTFS_ROOT, 2048u64, 4096u64)];
    let mut dev = build_gpt(512, 8192, 128, &parts);
    // Corrupt the entry array (LBA 2) without touching the header.
    let mut blk = [0u8; 512];
    dev.read_blocks(2, &mut blk).unwrap();
    blk[40] ^= 0x01;
    dev.write_blocks(2, &blk).unwrap();
    assert_eq!(
        parse_partition_table(&mut dev),
        Err(PartitionError::Gpt(gpt::GptError::EntriesCrc))
    );
}

#[test]
fn gpt_rejects_an_extent_past_the_device() {
    let parts = [(TYPE_GUID_RUSTFS_ROOT, 2048u64, 999_999u64)];
    let mut dev = build_gpt(512, 8192, 128, &parts);
    assert_eq!(
        parse_partition_table(&mut dev),
        Err(PartitionError::Gpt(gpt::GptError::BadExtent))
    );
}

// ----------------------------------------------------------------------
// Scheme dispatch
// ----------------------------------------------------------------------

#[test]
fn dispatch_reads_an_mbr_disk() {
    let parts = [boot(2048, 4096), root(6144, 4096)];
    let sector = mbr::encode(&parts).unwrap();
    let mut dev = VecBlock::new(512, 16_384);
    dev.put(0, &sector);

    let table = parse_partition_table(&mut dev).expect("parses MBR via dispatch");
    assert_eq!(
        table.first_of_type(PartitionType::RustFsRoot),
        Some(parts[1])
    );
}

#[test]
fn dispatch_reports_no_scheme_on_a_blank_disk() {
    let mut dev = VecBlock::new(512, 64);
    assert_eq!(
        parse_partition_table(&mut dev),
        Err(PartitionError::Mbr(MbrError::BadSignature))
    );
}

// ----------------------------------------------------------------------
// PartitionBlock window
// ----------------------------------------------------------------------

#[test]
fn window_translates_and_bounds_accesses() {
    let mut dev = VecBlock::new(512, 64);
    // Mark block 10 (the window's first block) with a sentinel.
    dev.put(10, &[0xab; 512]);

    let mut win = PartitionBlock::new(dev, 10, 4).expect("window fits");
    assert_eq!(win.block_count(), 4);
    assert_eq!(win.geometry().unwrap().block_count, 4);

    let mut buf = [0u8; 512];
    win.read_blocks(0, &mut buf).expect("reads window LBA 0");
    assert_eq!(buf, [0xab; 512]);

    // A read past the window end is refused.
    let mut over = [0u8; 512];
    assert_eq!(
        win.read_blocks(4, &mut over),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn window_rejects_an_extent_past_the_device() {
    let dev = VecBlock::new(512, 64);
    assert_eq!(
        PartitionBlock::new(dev, 60, 8).map(|_| ()).unwrap_err(),
        DriverError::LengthOutOfRange
    );
}

#[test]
fn window_from_partition_uses_the_extent() {
    let dev = VecBlock::new(512, 1024);
    let part = root(64, 128);
    let win = PartitionBlock::from_partition(dev, &part).expect("window");
    assert_eq!(win.block_count(), 128);
}

#[test]
fn table_push_fails_closed_past_the_cap() {
    let mut table = PartitionTable::empty();
    for _ in 0..MAX_PARTITIONS {
        table.push(root(1, 1)).expect("within cap");
    }
    assert_eq!(
        table.push(root(1, 1)),
        Err(PartitionError::TooManyPartitions)
    );
}

/// A known-answer vector for the IEEE CRC-32 the GPT path uses: the
/// check value of `"123456789"` is `0xCBF43926`.
#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(gpt::crc32(b"123456789"), 0xcbf4_3926);
}
