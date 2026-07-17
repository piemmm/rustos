//! Deterministic fuzz harness for the `lib/partition` table parsers
//! (a parser of untrusted on-disk bytes).
//!
//! A partition table is read off a disk that is outside RustOS's trust
//! boundary: a flashed SD card, a USB stick, or an attacker-supplied
//! image. A corrupt MBR signature, an overlapping or out-of-range extent,
//! a forged GPT header, a CRC that does not match, an entries-LBA that
//! escapes the device — all must be **rejected**, never trusted
//! (fail closed). Per ("every parser of untrusted
//! input ... has a fuzz target") the read path is driven here against
//! arbitrary disks, with a single invariant:
//!
//! * feeding any byte image to [`rustos_partition::parse_partition_table`]
//!   (and the lower [`rustos_partition::mbr::parse`] /
//!   [`rustos_partition::gpt::crc32`]) never panics and never reads out of
//!   bounds — the parser returns a validated [`rustos_partition::PartitionTable`]
//!   or a [`rustos_partition::PartitionError`]. The run
//!   aborting *is* the failure.
//!
//! RustOS pulls in no external fuzz runner: a
//! per-run-seeded LCG mutates valid seed images (a real MBR from
//! [`rustos_partition::mbr::encode`] and a CRC-correct GPT) and feeds pure
//! noise. A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep;
//! `cargo xtask fuzz` extends the loop to a wall-clock budget.

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::DriverError;
use rustos_partition::{gpt, mbr, parse_partition_table, Partition, PartitionType};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// An in-memory [`Block`] over a byte vector, mirroring the unit-test
/// mock; reads/writes fail closed on an out-of-range span.
struct VecBlock {
    data: Vec<u8>,
    block_size: u32,
}

impl VecBlock {
    fn new(data: Vec<u8>, block_size: u32) -> Self {
        Self { data, block_size }
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

/// A real, well-formed MBR disk image (sector 0 + slack) as a seed.
fn mbr_image() -> Vec<u8> {
    let parts = [
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: 2048,
            block_count: 4096,
        },
        Partition {
            ty: PartitionType::ARXFSRoot,
            start_lba: 6144,
            block_count: 4096,
        },
    ];
    let sector = mbr::encode(&parts).expect("seed MBR encodes");
    let mut img = vec![0u8; 512 * 64];
    img[..512].copy_from_slice(&sector);
    img
}

/// A CRC-correct GPT disk image as a seed (protective MBR + header +
/// entry array, one `ARXFS` root entry). Kept small so the mutation loop
/// clones it cheaply.
fn gpt_image() -> Vec<u8> {
    let bs = 512usize;
    let num_entries = 32u32;
    let blocks = 16usize;
    let mut img = vec![0u8; bs * blocks];

    // Protective MBR.
    img[mbr::PARTITION_TABLE_OFFSET + 4] = 0xee;
    img[bs - 2] = mbr::MBR_SIGNATURE[0];
    img[bs - 1] = mbr::MBR_SIGNATURE[1];

    // Entry array from LBA 2.
    let region_len = num_entries as usize * gpt::ENTRY_LEN;
    let mut region = vec![0u8; region_len];
    region[0..16].copy_from_slice(&gpt::TYPE_GUID_ARXFS_ROOT);
    region[16] = 1;
    region[32..40].copy_from_slice(&12u64.to_le_bytes());
    region[40..48].copy_from_slice(&14u64.to_le_bytes());
    let entries_crc = gpt::crc32(&region);
    img[2 * bs..2 * bs + region_len].copy_from_slice(&region);

    // Primary header at LBA 1.
    let hdr_off = bs;
    img[hdr_off..hdr_off + 8].copy_from_slice(&gpt::HEADER_SIGNATURE);
    img[hdr_off + 8..hdr_off + 12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    img[hdr_off + 12..hdr_off + 16].copy_from_slice(&92u32.to_le_bytes());
    img[hdr_off + 72..hdr_off + 80].copy_from_slice(&2u64.to_le_bytes());
    img[hdr_off + 80..hdr_off + 84].copy_from_slice(&num_entries.to_le_bytes());
    img[hdr_off + 84..hdr_off + 88]
        .copy_from_slice(&u32::try_from(gpt::ENTRY_LEN).expect("fits").to_le_bytes());
    img[hdr_off + 88..hdr_off + 92].copy_from_slice(&entries_crc.to_le_bytes());
    let header_crc = gpt::crc32(&img[hdr_off..hdr_off + 92]);
    img[hdr_off + 16..hdr_off + 20].copy_from_slice(&header_crc.to_le_bytes());

    img
}

/// `x` reduced into `0..=max`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// Parse `bytes` as a disk at both common logical-block sizes and drain
/// the result: must never panic, whatever the image.
fn exercise_never_panics(bytes: &[u8]) {
    for bs in [512u32, 4096u32] {
        if bytes.len() < bs as usize {
            continue;
        }
        // Truncate to a whole number of blocks.
        let usable = bytes.len() - (bytes.len() % bs as usize);
        let mut dev = VecBlock::new(bytes[..usable].to_vec(), bs);
        if let Ok(table) = parse_partition_table(&mut dev) {
            // Touch every accessor a caller would.
            let _ = table.first_of_type(PartitionType::FatBoot);
            let _ = table.first_of_type(PartitionType::ARXFSRoot);
            for p in table.partitions() {
                let _ = (p.ty, p.start_lba, p.block_count);
            }
        }
    }
    // The raw MBR sector parser, fed the first 512 bytes directly.
    if bytes.len() >= mbr::MBR_SECTOR_LEN {
        let _ = mbr::parse(&bytes[..mbr::MBR_SECTOR_LEN]);
    }
    // The CRC must accept any slice without panicking.
    let _ = gpt::crc32(bytes);
}

#[test]
fn parsing_any_partition_table_never_panics() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let corpus = [mbr_image(), gpt_image()];

    let mut state: u64 = rustos_fuzzseed::start(
        "parsing_any_partition_table_never_panics",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A real disk image with a handful of bytes flipped at random,
        //    hammering the signature, type bytes, LBAs, header, and CRCs.
        let template = &corpus[bounded(next(), corpus.len() - 1)];
        let mut mutated = template.clone();
        let flips = bounded(next(), 24);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise_never_panics(&mutated);

        // 2. A truncation of a real image, driving the bounds checks.
        let keep = bounded(next(), template.len());
        exercise_never_panics(&template[..keep]);

        // 3. Pure noise of an arbitrary length.
        let nlen = bounded(next(), 9000);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        exercise_never_panics(&noise);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
