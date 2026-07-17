//! Volume formatting (`mkfs`) for every ADFS variant.
//!
//! The formatter lays out an empty, structurally valid volume of the
//! chosen [`AdfsVariant`]: the free-space map (old or new), the boot
//! block where the variant carries one, and the root directory in the
//! variant's directory format.

use crate::bigdir::{BigDir, DirStore, BIG_DIR_GRAIN};
use crate::dir::{FixedDir, FixedFormat, NEW_DIR_SIZE, NEW_DIR_SIZE_U32};
use crate::disc::{
    boot_block_checksum, DiscRecord, BOOT_BLOCK_OFFSET, BOOT_BLOCK_SIZE, DISC_RECORD_IN_BOOT_BLOCK,
    DISC_RECORD_SIZE, FRAG_BAD, FRAG_ROOT,
};
use crate::newmap::{bits_set, bits_set_field, zone_check, MAX_ZONE_BYTES};
use crate::oldmap::{OldMap, OLD_SECTOR_SIZE};
use crate::volume::Volume;
use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;

/// Disc id stamped into freshly formatted volumes.
const FORMAT_DISC_ID: u16 = 0x5253; // "RS"

/// Byte length of the boot area reserved at the start of a multi-zone
/// volume (the boot block sits inside it at `0xC00`).
const BOOT_AREA_BYTES: u64 = 0x1000;

/// The ADFS on-disc format variants the driver formats and mounts.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AdfsVariant {
    /// 160 KiB, old map, 1280-byte `Hugo` directories.
    S,
    /// 320 KiB, old map, 1280-byte `Hugo` directories.
    M,
    /// 640 KiB, old map, 1280-byte `Hugo` directories.
    L,
    /// 800 KiB, old map, 2048-byte directories.
    D,
    /// 800 KiB, new map, 2048-byte directories.
    E,
    /// 800 KiB, new map, big directories (E+).
    EPlus,
    /// 1600 KiB, new map with boot block, 2048-byte directories.
    F,
    /// 1600 KiB, new map with boot block, big directories (F+).
    FPlus,
    /// New-map hard disc (boot block; size taken from the device),
    /// 2048-byte directories.
    HardDisc,
    /// New-map hard disc with big directories.
    HardDiscPlus,
}

impl AdfsVariant {
    /// Total volume size in bytes, or `None` for a hard disc (sized by
    /// the device).
    #[must_use]
    pub fn fixed_size(self) -> Option<u64> {
        match self {
            Self::S => Some(160 * 1024),
            Self::M => Some(320 * 1024),
            Self::L => Some(640 * 1024),
            Self::D | Self::E | Self::EPlus => Some(800 * 1024),
            Self::F | Self::FPlus => Some(1600 * 1024),
            Self::HardDisc | Self::HardDiscPlus => None,
        }
    }

    /// Whether the variant uses the old (sector 0–1) free-space map.
    #[must_use]
    pub fn is_old_map(self) -> bool {
        matches!(self, Self::S | Self::M | Self::L | Self::D)
    }

    /// Whether the variant uses big (E+/F+) directories.
    #[must_use]
    pub fn is_big_dir(self) -> bool {
        matches!(self, Self::EPlus | Self::FPlus | Self::HardDiscPlus)
    }
}

/// Build the disc record describing a fresh new-map volume of this
/// variant (`device_bytes` sizes a hard disc).
///
/// Floppy geometry uses the authentic `FileCore` parameters, under
/// which every zone's coverage fits the disc exactly (E) or leaves a
/// tail that is marked as a defect fragment (F). A hard disc derives
/// its zone count from the device size.
///
/// # Errors
///
/// * [`DriverError::Unsupported`] for an old-map variant.
/// * [`DriverError::NoSpace`] if the device is too small for the
///   variant.
pub(crate) fn new_map_record(
    variant: AdfsVariant,
    device_bytes: u64,
) -> Result<DiscRecord, DriverError> {
    let format_version = u32::from(variant.is_big_dir());
    let mut record = DiscRecord {
        log2secsize: 10,
        secspertrack: 5,
        heads: 2,
        density: 2,
        idlen: 15,
        log2bpmb: 7,
        skew: 1,
        bootoption: 0,
        lowsector: 0,
        nzones: 1,
        zone_spare: 0x520,
        root: 0,
        disc_size: 0,
        disc_id: 0,
        disc_name: *b"TAIRiX    ",
        disc_type: 0,
        log2sharesize: 0,
        big_flag: false,
        format_version,
        root_size: if format_version != 0 {
            BIG_DIR_GRAIN
        } else {
            0
        },
    };
    match variant {
        AdfsVariant::E | AdfsVariant::EPlus => {}
        AdfsVariant::F | AdfsVariant::FPlus => {
            record.secspertrack = 10;
            record.density = 4;
            record.log2bpmb = 6;
            record.nzones = 4;
            record.zone_spare = 0x640;
        }
        AdfsVariant::HardDisc | AdfsVariant::HardDiscPlus => {
            record.secspertrack = 63;
            record.heads = 16;
            record.density = 0;
            record.log2secsize = 9;
            record.zone_spare = 32;
            // Scale the map-bit size so the zone count stays modest on
            // a large device (one zone maps ~2 MiB at 512-byte bits).
            let mut log2bpmb = 9u8;
            while log2bpmb < 15 && (device_bytes >> log2bpmb) > 1 << 22 {
                log2bpmb += 1;
            }
            record.log2bpmb = log2bpmb;
            let zone0_bits = zone_stream_bits(&record, true);
            let later_zone_bits = zone_stream_bits(&record, false);
            let total_bits = device_bytes >> log2bpmb;
            let extra = total_bits.saturating_sub(zone0_bits);
            let nzones = 1 + extra.div_ceil(later_zone_bits);
            record.nzones = u16::try_from(nzones).map_err(|_| DriverError::NoSpace)?;
        }
        _ => return Err(DriverError::Unsupported),
    }
    let size = match variant.fixed_size() {
        Some(fixed) => fixed,
        None => device_bytes,
    };
    if size > device_bytes {
        return Err(DriverError::NoSpace);
    }
    // Round down to whole sectors, then trim so the map's coverage
    // excess is either zero or a representable defect fragment.
    let mut disc_size = size & !(record.sector_size() - 1);
    let coverage = map_coverage_bits(&record);
    loop {
        let bits = disc_size >> record.log2bpmb;
        if bits == 0 {
            return Err(DriverError::NoSpace);
        }
        let excess = coverage.saturating_sub(bits);
        if bits <= coverage && (excess == 0 || excess > u64::from(record.idlen)) {
            break;
        }
        disc_size -= record.sector_size();
    }
    record.disc_size = disc_size;
    Ok(record)
}

/// Map bits a zone's fragment stream holds (`zone 0` also embeds the
/// disc record).
pub(crate) fn zone_stream_bits(record: &DiscRecord, zone0: bool) -> u64 {
    let zone_size = u64::from(record.zone_bits());
    if zone0 {
        zone_size - (DISC_RECORD_SIZE as u64) * 8
    } else {
        zone_size
    }
}

/// Total disc blocks (map bits) the whole map can describe.
pub(crate) fn map_coverage_bits(record: &DiscRecord) -> u64 {
    zone_stream_bits(record, true) + u64::from(record.nzones - 1) * zone_stream_bits(record, false)
}

/// A [`DirStore`] window over a fixed byte range of the volume, used
/// while formatting (the root's location is known before the map is
/// mountable).
struct VolumeWindow<'a, B: Block> {
    volume: &'a mut Volume<B>,
    base: u64,
    len: u32,
}

impl<B: Block> DirStore for VolumeWindow<'_, B> {
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), DriverError> {
        if u64::from(offset) + buf.len() as u64 > u64::from(self.len) {
            return Err(DriverError::BadMagic);
        }
        self.volume.read_bytes(self.base + u64::from(offset), buf)
    }

    fn write_at(&mut self, offset: u32, data: &[u8]) -> Result<(), DriverError> {
        if u64::from(offset) + data.len() as u64 > u64::from(self.len) {
            return Err(DriverError::BadMagic);
        }
        self.volume.write_bytes(self.base + u64::from(offset), data)
    }
}

/// Format `volume` as an old-map variant (S/M/L/D).
fn format_old_map<B: Block>(
    volume: &mut Volume<B>,
    variant: AdfsVariant,
) -> Result<(), DriverError> {
    let size = variant.fixed_size().ok_or(DriverError::Unsupported)?;
    if size > volume.device_bytes() {
        return Err(DriverError::NoSpace);
    }
    // A floppy's sector count is far below `u32`.
    let disc_sectors = u32::try_from(size / OLD_SECTOR_SIZE).unwrap_or(0);
    let (format, root_sector) = match variant {
        AdfsVariant::D => (FixedFormat::New, 4u32),
        _ => (FixedFormat::Old, 2u32),
    };
    let root_offset = u64::from(root_sector) * OLD_SECTOR_SIZE;
    let root_sectors = u32::try_from(format.size() as u64 / OLD_SECTOR_SIZE).unwrap_or(0);
    // Zero the gap between the map and the root (a D-format volume is
    // recognised by the zero bytes at the start of sector 2).
    volume.zero_bytes(2 * OLD_SECTOR_SIZE, root_offset - 2 * OLD_SECTOR_SIZE)?;
    let mut map = OldMap::initialise(disc_sectors, root_sector + root_sectors, FORMAT_DISC_ID);
    map.store(volume)?;
    let root = FixedDir::initialise(format, *b"Nick", b"$", root_sector);
    volume.write_bytes(root_offset, &root.data[..format.size()])
}

/// Format `volume` as a new-map variant (E/E+/F/F+/hard disc).
fn format_new_map<B: Block>(
    volume: &mut Volume<B>,
    variant: AdfsVariant,
) -> Result<(), DriverError> {
    let mut record = new_map_record(variant, volume.device_bytes())?;
    record.disc_id = FORMAT_DISC_ID;
    let map_offset = record.map_offset();
    let map_bytes = record.map_size();
    let bpmb = record.bytes_per_map_bit();
    let min_frag_bytes = (u64::from(record.idlen) + 1) * bpmb;
    // Fragment 2 holds the boot area (multi-zone volumes) and the map
    // with its reserved second-copy area. On a fixed-directory volume
    // the root shares fragment 2 directly after the map (the authentic
    // E/F layout); on a big-directory volume the root gets fragment 3
    // of its own — it must be able to grow without spilling into the
    // boot area (the authentic E+/F+ layout).
    let system_blocks = (2 * map_bytes).div_ceil(bpmb).max(min_frag_bytes / bpmb);
    let root_size = if variant.is_big_dir() {
        record.root_size
    } else {
        NEW_DIR_SIZE_U32
    };
    let root_offset = map_offset + system_blocks * bpmb;
    let root_blocks = u64::from(root_size)
        .div_ceil(bpmb)
        .max(min_frag_bytes / bpmb);
    if variant.is_big_dir() {
        record.root = (FRAG_ROOT + 1) << 8 | 1;
    } else {
        let root_share = (root_offset - map_offset) / record.sector_size() + 1;
        if root_share > 0xFF {
            return Err(DriverError::NoSpace);
        }
        record.root = FRAG_ROOT << 8 | u32::try_from(root_share).unwrap_or(0);
    }
    record.validate()?;
    // The globally allocated extents, in disc blocks. The boot area,
    // like every fragment, must span at least one legal fragment; the
    // padding just reserves a little more of the disc's start.
    let boot_blocks = if record.nzones > 1 {
        BOOT_AREA_BYTES.div_ceil(bpmb).max(min_frag_bytes / bpmb)
    } else {
        0
    };
    let system_start = map_offset / bpmb;
    if root_offset + u64::from(root_size) > record.disc_size {
        return Err(DriverError::NoSpace);
    }
    let layout = MapLayout {
        boot_blocks,
        system_start,
        // A fixed-directory root belongs to the system fragment.
        system_blocks: if variant.is_big_dir() {
            system_blocks
        } else {
            system_blocks + root_blocks
        },
        root_blocks: if variant.is_big_dir() { root_blocks } else { 0 },
    };
    write_fresh_map(volume, &record, &layout)?;
    // Zero the reserved second map copy area.
    volume.zero_bytes(map_offset + map_bytes, map_bytes)?;
    if record.nzones > 1 {
        write_boot_block(volume, &record)?;
    }
    if variant.is_big_dir() {
        let mut window = VolumeWindow {
            volume,
            base: root_offset,
            len: root_size,
        };
        BigDir::initialise(&mut window, root_size, b"$", record.root)?;
    } else {
        let root = FixedDir::initialise(FixedFormat::New, *b"Nick", b"$", record.root);
        volume.write_bytes(root_offset, &root.data[..NEW_DIR_SIZE])?;
    }
    Ok(())
}

/// The allocated extents a fresh volume carries, in disc blocks.
struct MapLayout {
    /// Boot-area blocks at the start of the disc (fragment 2).
    boot_blocks: u64,
    /// First block of the map area (fragment 2, and the root behind it).
    system_start: u64,
    /// Blocks of the map area (including a fixed-directory root).
    system_blocks: u64,
    /// Blocks of a big-directory root (fragment 3), directly after the
    /// system extent; `0` on fixed-directory volumes.
    root_blocks: u64,
}

/// Write every map zone of a fresh volume: the allocated system
/// extents, one free fragment per gap, and the trailing defect
/// fragment covering map bits past the end of the disc.
fn write_fresh_map<B: Block>(
    volume: &mut Volume<B>,
    record: &DiscRecord,
    layout: &MapLayout,
) -> Result<(), DriverError> {
    // The sector size was validated against `MAX_ZONE_BYTES`.
    let sector = usize::try_from(record.sector_size()).unwrap_or(MAX_ZONE_BYTES);
    let idlen = u32::from(record.idlen);
    let dr_bits = (DISC_RECORD_SIZE as u64) * 8;
    let zone_size = u64::from(record.zone_bits());
    let total_blocks = record.disc_size / record.bytes_per_map_bit();
    for zone in 0..u64::from(record.nzones) {
        let mut buf = [0u8; MAX_ZONE_BYTES];
        let (start_bit, start_block) = if zone == 0 {
            (bit_offset(32 + dr_bits), 0u64)
        } else {
            (32u32, zone * zone_size - dr_bits)
        };
        let physical_end = bit_offset(32 + zone_size);
        let covered = u64::from(physical_end - start_bit);
        let logical_blocks = total_blocks.saturating_sub(start_block).min(covered);
        let logical_end = start_bit + bit_offset(logical_blocks);
        // Zone 0 embeds the disc record.
        if zone == 0 {
            buf[4..4 + DISC_RECORD_SIZE].copy_from_slice(&record.encode());
            buf[3] = 0xFF; // Cross-check bytes XOR to 0xFF across zones.
        }
        // Collect this zone's allocated segments in block order. Every
        // segment arrives at least `idlen + 1` map bits long (the
        // layout sizes them so), and abutting segments are legal —
        // adjacent fragments are self-delimiting.
        let min_bits = u64::from(idlen) + 1;
        let mut segments = [(0u64, 0u64, 0u32); 3];
        let mut segment_count = 0;
        for (seg_start, seg_len, id) in [
            (0u64, layout.boot_blocks, FRAG_ROOT),
            (layout.system_start, layout.system_blocks, FRAG_ROOT),
            (
                layout.system_start + layout.system_blocks,
                layout.root_blocks,
                FRAG_ROOT + 1,
            ),
        ] {
            if seg_len == 0 {
                continue;
            }
            let lo = seg_start.max(start_block);
            let hi = (seg_start + seg_len).min(start_block + logical_blocks);
            if lo < hi {
                // A segment split across zones must leave a legal
                // fragment in each; the layout keeps extents whole-zone
                // sized or well inside one zone, so a short piece here
                // is a layout bug caught closed.
                if hi - lo < min_bits {
                    return Err(DriverError::NoSpace);
                }
                segments[segment_count] = (lo - start_block, hi - lo, id);
                segment_count += 1;
            }
        }
        // Lay the stream: gaps become free fragments (linked from the
        // zone header), segments become allocated fragments.
        let mut prev_link_pos: Option<u32> = None;
        let mut cursor = start_bit;
        let place_free =
            |buf: &mut [u8; MAX_ZONE_BYTES], prev: &mut Option<u32>, from: u32, to: u32| {
                if to > from {
                    link_free(buf, record, *prev, from);
                    bits_set(buf, to - 1, true);
                    *prev = Some(from);
                }
            };
        for &(rel_start, len, id) in &segments[..segment_count] {
            let seg_bit = start_bit + bit_offset(rel_start);
            // A free gap smaller than a legal fragment cannot exist.
            if seg_bit != cursor && seg_bit - cursor < bit_offset(min_bits) {
                return Err(DriverError::NoSpace);
            }
            place_free(&mut buf, &mut prev_link_pos, cursor, seg_bit);
            bits_set_field(&mut buf, seg_bit, idlen, id);
            bits_set(&mut buf, seg_bit + bit_offset(len) - 1, true);
            cursor = seg_bit + bit_offset(len);
        }
        // The trailing free gap, and the defect run covering map bits
        // past the disc end. A too-small trailing gap is folded into
        // whichever neighbour keeps every fragment legal.
        let mut defect_start = logical_end;
        if logical_end > cursor && u64::from(logical_end - cursor) < min_bits {
            if physical_end > logical_end {
                // The defect run absorbs the sliver of real blocks.
                defect_start = cursor;
            } else if segment_count > 0 {
                // The last allocated fragment absorbs it as slack.
                bits_set(&mut buf, cursor - 1, false);
                bits_set(&mut buf, logical_end - 1, true);
                cursor = logical_end;
            } else {
                return Err(DriverError::NoSpace);
            }
        }
        place_free(
            &mut buf,
            &mut prev_link_pos,
            cursor,
            defect_start.min(logical_end),
        );
        if physical_end > defect_start.min(logical_end).max(cursor) {
            let at = defect_start.min(logical_end).max(cursor);
            if physical_end - at < bit_offset(min_bits) {
                return Err(DriverError::NoSpace);
            }
            bits_set_field(&mut buf, at, idlen, FRAG_BAD);
            bits_set(&mut buf, physical_end - 1, true);
        }
        buf[0] = zone_check(&buf[..sector]);
        let at = record.map_offset() + zone * record.sector_size();
        volume.write_bytes(at, &buf[..sector])?;
    }
    Ok(())
}

/// A map-bit offset as `u32`: every zone is a single sector, so its
/// bit offsets stay far below `u32` by construction.
fn bit_offset(bits: u64) -> u32 {
    u32::try_from(bits).unwrap_or(u32::MAX)
}

/// Point the free-list link at `prev` (a free fragment's bit, or the
/// zone header) at the free fragment starting at `bit`, writing the
/// header's stop bit when the link is the header's.
fn link_free(buf: &mut [u8; MAX_ZONE_BYTES], record: &DiscRecord, prev: Option<u32>, bit: u32) {
    let link_bits = u32::from(record.idlen).min(15);
    match prev {
        None => {
            // The 16-bit header link carries its own stop bit at the top.
            bits_set_field(buf, 8, link_bits, bit - 8);
            bits_set(buf, 23, true);
        }
        Some(pos) => bits_set_field(buf, pos, link_bits, bit - pos),
    }
}

/// Write the checksummed boot block embedding the disc record.
fn write_boot_block<B: Block>(
    volume: &mut Volume<B>,
    record: &DiscRecord,
) -> Result<(), DriverError> {
    let mut block = [0u8; BOOT_BLOCK_SIZE];
    block[DISC_RECORD_IN_BOOT_BLOCK..DISC_RECORD_IN_BOOT_BLOCK + DISC_RECORD_SIZE]
        .copy_from_slice(&record.encode());
    block[BOOT_BLOCK_SIZE - 1] = boot_block_checksum(&block);
    volume.write_bytes(BOOT_BLOCK_OFFSET, &block)
}

/// Format `volume` as `variant`, laying out an empty, valid filesystem.
///
/// # Errors
///
/// * [`DriverError::NoSpace`] if the device is too small.
/// * [`DriverError::DeviceFault`] on an unrecoverable block access.
pub(crate) fn format_volume<B: Block>(
    volume: &mut Volume<B>,
    variant: AdfsVariant,
) -> Result<(), DriverError> {
    if variant.is_old_map() {
        format_old_map(volume, variant)
    } else {
        format_new_map(volume, variant)
    }
}
