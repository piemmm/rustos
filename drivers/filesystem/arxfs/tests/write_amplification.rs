//! The write path's device-command ledger, and the baseline it measures.
//!
//! A copy-on-write filesystem puts rather more than one byte on the device for
//! every byte a caller writes: the data record, the extent leaf that maps it,
//! the tree spine above that leaf, the allocation-map page, the transaction
//! root, and the superblock slot that publishes it — each mirrored, each
//! sealed. How much more is a number, and it is the number the write-back
//! stages exist to reduce (`plans/ARXFS-WRITEBACK.md`), so it is measured here
//! rather than asserted by inspection or quoted from a note.
//!
//! The fixture is an in-RAM device that records every command it is issued, in
//! order: each write's start block and run length, and each cache barrier. One
//! ledger yields all four figures a write path is judged on — the commands it
//! costs, the blocks those commands carry, how many of those blocks a later
//! write in the same window supersedes, and how the run lengths are
//! distributed — plus the issue order a durability barrier is proved by.
//!
//! Three of the properties asserted here are measurements of the present, not
//! endorsements of it, and each is the acceptance hook of a named later stage.
//! A transaction rewrites the same metadata block once per data block it
//! stores; sixteen calls writing one payload cost far more than one call
//! writing it; every write carries exactly one block, because the single-block
//! `write_block` is the driver's only device-write site while the read path
//! already gathers runs. The baseline also records that a commit issues no
//! barrier at all — the durability defect filed as `plans/OPEN-DEFECTS.md`
//! D63. Each stage that closes one of these changes the figures below, in the
//! change that earns it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use tairix_abi::driver::block::{Block, BlockGeometry, DiscardCapability};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};
use tairix_fuzzseed::Lcg;

/// One command the driver issued to the device, in issue order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    /// A write of `blocks` contiguous blocks starting at `lba`.
    Write { lba: u64, blocks: u64 },
    /// A device-cache barrier: everything written before it reaches stable
    /// media before anything written after it.
    Barrier,
}

/// Every command a measured window issued, in order.
#[derive(Default)]
struct Ledger {
    commands: Vec<Command>,
}

impl Ledger {
    /// Drop the recorded commands, opening a fresh measurement window.
    fn arm(&mut self) {
        self.commands.clear();
    }

    /// How many writes carried each run length: the shape a coalescer changes
    /// and a per-command device is priced by.
    fn run_lengths(&self) -> BTreeMap<u64, usize> {
        let mut histogram = BTreeMap::new();
        for command in &self.commands {
            if let Command::Write { blocks, .. } = command {
                *histogram.entry(*blocks).or_insert(0_usize) += 1;
            }
        }
        histogram
    }
}

/// What one measured window cost the device.
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    /// Write commands — one bus transaction each, and the figure a per-command
    /// device (an SD card's `CMD24`) is priced by.
    writes: usize,
    /// Blocks those commands carried in total.
    blocks: u64,
    /// Distinct block addresses among them. The shortfall against `blocks` is
    /// the window's rewrite churn: bytes sealed and sent whose only surviving
    /// version is the last one.
    distinct_blocks: u64,
    /// Bytes those commands carried in total.
    bytes: u64,
    /// Cache barriers issued.
    barriers: usize,
}

impl Cost {
    /// Total the ledger's commands at `block_size` bytes a block.
    fn of(ledger: &Ledger, block_size: u32) -> Self {
        let mut writes = 0;
        let mut blocks = 0;
        let mut barriers = 0;
        let mut addresses = BTreeSet::new();
        for command in &ledger.commands {
            match *command {
                Command::Write { lba, blocks: run } => {
                    writes += 1;
                    blocks += run;
                    addresses.extend(lba..lba + run);
                }
                Command::Barrier => barriers += 1,
            }
        }
        Self {
            writes,
            blocks,
            distinct_blocks: u64::try_from(addresses.len()).expect("a block count fits a u64"),
            bytes: blocks * u64::from(block_size),
            barriers,
        }
    }

    /// Block writes a later write in the same window superseded.
    fn superseded(&self) -> u64 {
        self.blocks - self.distinct_blocks
    }

    /// Hundredths of a byte on the device per byte the caller asked to write —
    /// the write amplification, exactly, without rounding through a float. A
    /// metadata-only operation writes no payload and so has no ratio.
    fn amplification_hundredths(&self, payload: u64) -> Option<u64> {
        (payload > 0).then(|| self.bytes * 100 / payload)
    }
}

/// In-RAM device that records every command it is issued.
///
/// It stores only the blocks actually written; an absent block reads as zeroes,
/// exactly as a freshly provisioned device does. That is what lets the same
/// fixture price one workload on a small volume and on a volume far larger than
/// the host's RAM, which is the only way to show the figures below are
/// properties of the write path and not of the device it was measured on.
struct LedgerBlock {
    blocks: BTreeMap<u64, Vec<u8>>,
    block_size: u32,
    block_count: u64,
    ledger: Rc<RefCell<Ledger>>,
}

impl LedgerBlock {
    fn new(block_size: u32, block_count: u64, ledger: &Rc<RefCell<Ledger>>) -> Self {
        Self {
            blocks: BTreeMap::new(),
            block_size,
            block_count,
            ledger: Rc::clone(ledger),
        }
    }

    /// How many blocks a `len`-byte request at `lba` covers, or a typed refusal
    /// for one the device cannot serve.
    fn run(&self, lba: u64, len: usize) -> Result<u64, DriverError> {
        let bs = self.block_size as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = u64::try_from(len / bs).expect("a run length fits a u64");
        if lba.saturating_add(blocks) > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(blocks)
    }
}

impl Block for LedgerBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.run(lba, buf.len())?;
        for (index, chunk) in buf.chunks_mut(self.block_size as usize).enumerate() {
            let at = lba + u64::try_from(index).expect("a run length fits a u64");
            match self.blocks.get(&at) {
                Some(stored) => chunk.copy_from_slice(stored),
                None => chunk.fill(0),
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let blocks = self.run(lba, buf.len())?;
        self.ledger
            .borrow_mut()
            .commands
            .push(Command::Write { lba, blocks });
        for (index, chunk) in buf.chunks(self.block_size as usize).enumerate() {
            let at = lba + u64::try_from(index).expect("a run length fits a u64");
            self.blocks.insert(at, chunk.to_vec());
        }
        Ok(())
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(DiscardCapability::unsupported())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.ledger.borrow_mut().commands.push(Command::Barrier);
        Ok(())
    }
}

/// Volume key every fixture is formatted under. ARXFS is encrypted by default
/// and sealing is part of what a write costs, so there is no plaintext variant
/// to measure against.
const TEST_KEY: VolumeKey = [0x71; VOLUME_KEY_LEN];

/// Deterministic stand-in for the platform RNG seam. Test scaffolding only.
struct TestEntropy(u8);

impl EntropySource for TestEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
        Ok(())
    }
}

/// Device size the baseline is measured on: room for the payload, the trees
/// mapping it, and the superseded copies each transaction leaves behind.
const DEVICE_BYTES: u64 = 8 << 20;

/// Device size the floor case is measured on — a hundred tebibytes, thirteen
/// million times the baseline's. Nothing materialises it: the fixture stores
/// only the blocks written, and a volume this size is exactly what ARXFS has to
/// serve from a machine that could not hold a millionth of it.
const FLOOR_DEVICE_BYTES: u64 = 100 << 40;

/// Payload every data workload writes.
const PAYLOAD_BYTES: usize = 64 << 10;

/// Call size the chunked workload splits that payload into.
const CHUNK_BYTES: usize = 4 << 10;

/// Bytes the small-append workload writes, and the bytes the file already holds
/// when it does.
const APPEND_BYTES: usize = 34;

/// The file every workload writes to.
const FILE: &[u8] = b"payload";

/// Pseudo-random payload bytes: the compressor declines them, so a window
/// prices real stored bytes rather than a codec's luck, and the fixed seed
/// keeps the figures reproducible.
fn payload(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    Lcg::new(0x5741_4d50).fill(&mut bytes);
    bytes
}

/// A freshly formatted `device_bytes` volume of `block_size`-byte blocks, and
/// the ledger recording what its driver issues.
fn volume(block_size: u32, device_bytes: u64) -> (ARXFS<LedgerBlock>, Rc<RefCell<Ledger>>) {
    let ledger = Rc::new(RefCell::new(Ledger::default()));
    let device = LedgerBlock::new(block_size, device_bytes / u64::from(block_size), &ledger);
    let fs = ARXFS::format(device, 64, &TEST_KEY, &mut TestEntropy(0x2f)).expect("format");
    (fs, ledger)
}

/// The workloads the baseline is measured over: one write call against many for
/// the same bytes, the small append that is almost all metadata, and the
/// metadata-only create.
#[derive(Clone, Copy, Debug)]
enum Workload {
    /// The whole payload in one `write_at`.
    OneCall,
    /// The same payload in [`CHUNK_BYTES`]-byte calls.
    Chunked,
    /// A small append onto a file that already holds one.
    SmallAppend,
    /// Creating one empty file.
    CreateFile,
}

impl Workload {
    /// Bytes the caller asked to write inside the measured window.
    fn payload_bytes(self) -> u64 {
        match self {
            Self::OneCall | Self::Chunked => PAYLOAD_BYTES as u64,
            Self::SmallAppend => APPEND_BYTES as u64,
            Self::CreateFile => 0,
        }
    }

    /// Bring the volume to the state the measured window starts from, outside
    /// that window.
    fn prepare(self, fs: &mut ARXFS<LedgerBlock>) {
        if matches!(self, Self::CreateFile) {
            return;
        }
        let root = fs.root();
        fs.create(root, FILE, NodeKind::RegularFile)
            .expect("create");
        if matches!(self, Self::SmallAppend) {
            let head = &payload(2 * APPEND_BYTES)[..APPEND_BYTES];
            assert_eq!(fs.write_at(root, FILE, 0, head), Ok(APPEND_BYTES));
        }
    }

    /// The measured work itself.
    fn run(self, fs: &mut ARXFS<LedgerBlock>) {
        let root = fs.root();
        match self {
            Self::OneCall => {
                let body = payload(PAYLOAD_BYTES);
                assert_eq!(fs.write_at(root, FILE, 0, &body), Ok(PAYLOAD_BYTES));
            }
            Self::Chunked => {
                let body = payload(PAYLOAD_BYTES);
                for (call, chunk) in body.chunks(CHUNK_BYTES).enumerate() {
                    let at = u64::try_from(call * CHUNK_BYTES).expect("an offset fits a u64");
                    assert_eq!(fs.write_at(root, FILE, at, chunk), Ok(chunk.len()));
                }
            }
            Self::SmallAppend => {
                // Bytes the head does not already hold, so the append cannot
                // dedupe against it and the window prices a real store.
                let body = payload(2 * APPEND_BYTES);
                let at = u64::try_from(APPEND_BYTES).expect("an offset fits a u64");
                assert_eq!(
                    fs.write_at(root, FILE, at, &body[APPEND_BYTES..]),
                    Ok(APPEND_BYTES)
                );
            }
            Self::CreateFile => {
                fs.create(root, FILE, NodeKind::RegularFile)
                    .expect("create");
            }
        }
    }

    /// Run this workload on a fresh volume of that geometry, and total what the
    /// measured window cost the device.
    fn measure(self, block_size: u32, device_bytes: u64) -> (Cost, BTreeMap<u64, usize>) {
        let (mut fs, ledger) = volume(block_size, device_bytes);
        self.prepare(&mut fs);
        ledger.borrow_mut().arm();
        self.run(&mut fs);
        let held = ledger.borrow();
        (Cost::of(&held, block_size), held.run_lengths())
    }
}

/// One measured row of the baseline.
struct Row {
    workload: Workload,
    block_size: u32,
    /// Write amplification in hundredths; `None` for a metadata-only workload.
    amplification: Option<u64>,
    cost: Cost,
}

/// What the write path costs the device today: one command per block, no
/// barrier, and a transaction that rewrites its metadata once per data block it
/// stores.
///
/// A change to any figure here is a real change in what a write costs a device,
/// so the stage that improves one updates the row it improved, and a change
/// that did not mean to touch the write path is told that it did.
const BASELINE: &[Row] = &[
    // 64 KiB in one call. 148 of the 746 blocks are the payload's own data
    // records (443 content bytes fit a 512-byte block); the other 598 are
    // metadata, and 294 of those are superseded before the transaction ends.
    Row {
        workload: Workload::OneCall,
        block_size: 512,
        amplification: Some(582),
        cost: Cost {
            writes: 746,
            blocks: 746,
            distinct_blocks: 452,
            bytes: 381_952,
            barriers: 0,
        },
    },
    // The same bytes in sixteen calls: sixteen transactions, each republishing
    // the spine, a fresh root, and a ring slot, and each rewriting the
    // partially filled block the previous call left.
    Row {
        workload: Workload::Chunked,
        block_size: 512,
        amplification: Some(924),
        cost: Cost {
            writes: 1_183,
            blocks: 1_183,
            distinct_blocks: 331,
            bytes: 605_696,
            barriers: 0,
        },
    },
    // A wider block holds a wider node, so the same payload maps through a far
    // shallower tree and the command count falls eightfold — while the *bytes*
    // barely move, which is what makes this amplification structural rather
    // than granular.
    Row {
        workload: Workload::OneCall,
        block_size: 4096,
        amplification: Some(556),
        cost: Cost {
            writes: 89,
            blocks: 89,
            distinct_blocks: 57,
            bytes: 364_544,
            barriers: 0,
        },
    },
    Row {
        workload: Workload::Chunked,
        block_size: 4096,
        amplification: Some(1_775),
        cost: Cost {
            writes: 284,
            blocks: 284,
            distinct_blocks: 140,
            bytes: 1_163_264,
            barriers: 0,
        },
    },
    // 34 bytes appended: one rewritten data block, and twelve blocks of tree,
    // root, and ring slot to name it.
    Row {
        workload: Workload::SmallAppend,
        block_size: 512,
        amplification: Some(19_576),
        cost: Cost {
            writes: 13,
            blocks: 13,
            distinct_blocks: 13,
            bytes: 6_656,
            barriers: 0,
        },
    },
    // The floor: what an operation carrying no payload at all still costs.
    Row {
        workload: Workload::CreateFile,
        block_size: 512,
        amplification: None,
        cost: Cost {
            writes: 18,
            blocks: 18,
            distinct_blocks: 14,
            bytes: 9_216,
            barriers: 0,
        },
    },
];

#[test]
fn the_write_path_costs_its_measured_baseline() {
    for row in BASELINE {
        let (cost, _) = row.workload.measure(row.block_size, DEVICE_BYTES);
        assert_eq!(
            cost, row.cost,
            "{:?} at a {}-byte block size now costs {cost:?}",
            row.workload, row.block_size
        );
        assert_eq!(
            cost.amplification_hundredths(row.workload.payload_bytes()),
            row.amplification,
            "{:?} at a {}-byte block size put {} bytes on the device for {} \
             written",
            row.workload,
            row.block_size,
            cost.bytes,
            row.workload.payload_bytes()
        );
    }
}

/// Every write the driver issues carries exactly one block, however many
/// adjacent blocks it has to hand — including the two identical copies of a
/// mirrored metadata block, which are adjacent by construction.
///
/// This is the assertion the run coalescer moves: draining in ascending
/// physical order and gathering adjacent blocks weights this histogram toward
/// the staging window, and makes a mirror pair one two-block command.
#[test]
fn every_device_write_carries_exactly_one_block() {
    for row in BASELINE {
        let (cost, run_lengths) = row.workload.measure(row.block_size, DEVICE_BYTES);
        assert_eq!(
            run_lengths,
            BTreeMap::from([(1, cost.writes)]),
            "{:?} at a {}-byte block size issued runs {run_lengths:?}",
            row.workload,
            row.block_size
        );
    }
}

/// A transaction rewrites its metadata far more often than it need: every stored
/// block ends in a B-tree insert that copy-on-writes the covering leaf and sends
/// it, and its mirror, to the device at once — so the same leaf, the same spine,
/// and the same allocation-map page go out once per data block, and only the
/// last version of each survives the transaction.
///
/// Each data block is written once and is never superseded, being a fresh
/// copy-on-write allocation, so the churn measured here is metadata to the
/// block — the churn a transaction-scoped dirty set absorbs. The exact figures
/// are the baseline's; what this asserts is that the churn outweighs the payload
/// that provoked it.
#[test]
fn a_transaction_rewrites_the_same_metadata_block_repeatedly() {
    /// Data blocks 64 KiB of incompressible payload occupies at a 512-byte
    /// block size, cross-checked below against the driver's own accounting.
    const DATA_BLOCKS: u64 = 148;

    let (mut fs, ledger) = volume(512, DEVICE_BYTES);
    Workload::OneCall.prepare(&mut fs);
    ledger.borrow_mut().arm();
    Workload::OneCall.run(&mut fs);
    let cost = Cost::of(&ledger.borrow(), 512);

    let node = fs.lookup(fs.root(), FILE).expect("look the file up");
    let info = fs.node_info(node).expect("stat the file");
    assert_eq!(
        info.allocated,
        DATA_BLOCKS * 512,
        "the payload must really occupy {DATA_BLOCKS} data blocks"
    );

    assert!(
        cost.distinct_blocks > DATA_BLOCKS,
        "the payload's {DATA_BLOCKS} blocks must be mapped by metadata blocks \
         beyond them, not by nothing: {cost:?}"
    );
    assert!(
        cost.superseded() > DATA_BLOCKS,
        "the churn must outweigh the payload: {} superseded writes against \
         {DATA_BLOCKS} data blocks",
        cost.superseded()
    );
}

/// Splitting one payload across sixteen calls costs half again the commands of
/// writing it in one, for identical bytes: each call is its own transaction, so
/// each republishes the tree spine, a fresh root, and a ring slot, and each
/// rewrites the partly filled block its predecessor left.
///
/// This is the assertion the commit scheduler moves — a transaction the next
/// operation joins converges the two costs.
#[test]
fn chunking_one_payload_into_many_calls_costs_far_more_than_one_call() {
    for block_size in [512_u32, 4096] {
        let (one_call, _) = Workload::OneCall.measure(block_size, DEVICE_BYTES);
        let (chunked, _) = Workload::Chunked.measure(block_size, DEVICE_BYTES);
        assert!(
            chunked.writes > one_call.writes * 3 / 2,
            "at a {block_size}-byte block size, {} chunked commands against \
             {} for one call",
            chunked.writes,
            one_call.writes
        );
    }
}

/// The same write costs the same commands on a hundred-tebibyte volume as on an
/// eight-mebibyte one: thirteen million times the device, and not one extra
/// block sealed or sent.
///
/// This is what makes the baseline a property of the write path rather than of
/// the device it happened to be measured on, and it is the floor ARXFS is held
/// to — a machine that could not hold a millionth of a volume still has to
/// write to it at a cost set by the working set, never by the volume.
#[test]
fn the_cost_of_a_write_does_not_grow_with_the_volume() {
    for workload in [
        Workload::OneCall,
        Workload::Chunked,
        Workload::SmallAppend,
        Workload::CreateFile,
    ] {
        let (small, small_runs) = workload.measure(4096, DEVICE_BYTES);
        let (huge, huge_runs) = workload.measure(4096, FLOOR_DEVICE_BYTES);
        assert_eq!(
            huge, small,
            "{workload:?} costs {huge:?} on a {FLOOR_DEVICE_BYTES}-byte volume \
             against {small:?} on a {DEVICE_BYTES}-byte one"
        );
        assert_eq!(huge_runs, small_runs, "{workload:?} run lengths moved");
    }
}
