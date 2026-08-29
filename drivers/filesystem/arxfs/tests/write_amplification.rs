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
//! One property asserted here is a measurement of the present, not an
//! endorsement of it, and it is the acceptance hook of a named later stage:
//! sixteen calls writing one payload still cost far more than one call writing
//! it, which the commit scheduler converges. The rest is the write-back cache's
//! contract, held here: a transaction writes each block it touches once, the
//! drain hands the device one request per physical run rather than one per
//! block, and every commit issues exactly one barrier with nothing but the two
//! copies of its publishing superblock slot after it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use tairix_abi::driver::block::{Block, BlockGeometry, DiscardCapability};
use tairix_abi::driver::filesystem::{
    FilesystemRead, FilesystemWrite, NodeId, NodeKind, WritebackHost,
};
use tairix_abi::driver::DriverHandle;
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, RUN_BYTES, VOLUME_KEY_LEN};
use tairix_fuzzseed::Lcg;
use tairix_reclaim::{
    CacheBudget, FreeMemorySource, MemoryPressure, PinnedAccounting, PinnedLedger, PinnedShare,
    PressureBand, ReclaimOwner, ReportedPressure,
};

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
    /// Blocks the window read. Counted apart from `commands` because a read
    /// costs the device nothing a write ledger prices, but a *path* that reads
    /// more of the volume than its working set is exactly the amplification
    /// the scaling assertions bound.
    read_blocks: u64,
}

impl Ledger {
    /// Drop the recorded commands, opening a fresh measurement window.
    fn arm(&mut self) {
        self.commands.clear();
        self.read_blocks = 0;
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
    /// device is priced by: an SD card takes a run as one multi-block `CMD25`
    /// where each block on its own would be a `CMD24` and a completion wait.
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
        let blocks = self.run(lba, buf.len())?;
        self.ledger.borrow_mut().read_blocks += blocks;
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

/// How the volume under measurement publishes what it writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Publish {
    /// No write-back timer, so there is no window to age a transaction
    /// against and every operation publishes its own.
    PerOperation,
    /// A timer whose clock never advances, so the window never elapses and
    /// the operations of a measured window all join one transaction.
    Batched,
}

/// A write-back timer whose clock never advances, so a measured burst is
/// never split by its window and the measurement prices the batch itself.
/// It records nothing: what is being measured is the device traffic, and the
/// deadline the driver reports has no consumer here.
struct FrozenTimer;

impl WritebackHost for FrozenTimer {
    fn now_ns(&self) -> Option<u64> {
        Some(0)
    }

    fn writeback_due(&self, _volume: DriverHandle, _deadline_ns: Option<u64>) {}
}

static FROZEN_TIMER: FrozenTimer = FrozenTimer;

/// A freshly formatted `device_bytes` volume of `block_size`-byte blocks, and
/// the ledger recording what its driver issues.
fn volume(
    block_size: u32,
    device_bytes: u64,
    publish: Publish,
) -> (ARXFS<LedgerBlock>, Rc<RefCell<Ledger>>) {
    let ledger = Rc::new(RefCell::new(Ledger::default()));
    let device = LedgerBlock::new(block_size, device_bytes / u64::from(block_size), &ledger);
    let fs = ARXFS::format(device, 64, &TEST_KEY, &mut TestEntropy(0x2f)).expect("format");
    let fs = match publish {
        Publish::PerOperation => fs,
        Publish::Batched => fs.with_writeback_host(
            DriverHandle::from_raw(1).expect("a non-zero test handle"),
            &FROZEN_TIMER,
        ),
    };
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
    ///
    /// A batched volume is brought to a published state before the window is
    /// armed and handed on at its end, so the window prices the workload's own
    /// writes and the one commit that publishes them — never the fixture's.
    fn measure(
        self,
        block_size: u32,
        device_bytes: u64,
        publish: Publish,
    ) -> (Cost, BTreeMap<u64, usize>) {
        let (mut fs, ledger) = volume(block_size, device_bytes, publish);
        self.prepare(&mut fs);
        if publish == Publish::Batched {
            fs.flush()
                .expect("the fixture starts from a published volume");
        }
        ledger.borrow_mut().arm();
        self.run(&mut fs);
        if publish == Publish::Batched {
            fs.into_block()
                .expect("handing the volume on publishes the batch");
        }
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
    /// How many commands carried each run length — the shape the coalescer
    /// produces, as `(blocks, commands)` pairs.
    runs: &'static [(u64, usize)],
}

/// What the write path costs the device: one command per physical run, one
/// barrier per commit, and each block a transaction touches written exactly
/// once.
///
/// A change to any figure here is a real change in what a write costs a device,
/// so the stage that improves one updates the row it improved, and a change
/// that did not mean to touch the write path is told that it did.
const BASELINE: &[Row] = &[
    // 64 KiB in one call. 148 of the 158 blocks are the payload's own data
    // records (443 content bytes fit a 512-byte block); the other ten are five
    // mirrored metadata blocks — the extent and inode trees, the transaction
    // root, and the ring slot — each written once however many times the
    // transaction rewrote it. Five commands carry them: the payload's
    // consecutive data blocks as a 128-block run (the transfer bound) and a
    // 20-block remainder, the four mirrored metadata pairs as one 8-block run,
    // and the ring slot's two copies after the barrier.
    Row {
        workload: Workload::OneCall,
        block_size: 512,
        amplification: Some(123),
        cost: Cost {
            writes: 5,
            blocks: 158,
            distinct_blocks: 158,
            bytes: 80_896,
            barriers: 1,
        },
        runs: &[(1, 2), (8, 1), (20, 1), (128, 1)],
    },
    // The same bytes in sixteen calls: sixteen transactions, each republishing
    // the spine, a fresh root, and a ring slot, and each rewriting the
    // partially filled block the previous call left. The churn that survives is
    // therefore *between* transactions, which is what the commit scheduler
    // closes; within each one, every block is still written once, and each
    // transaction's blocks leave as two runs plus its slot pair.
    Row {
        workload: Workload::Chunked,
        block_size: 512,
        amplification: Some(261),
        cost: Cost {
            writes: 64,
            blocks: 335,
            distinct_blocks: 311,
            bytes: 171_520,
            barriers: 16,
        },
        runs: &[(1, 32), (8, 11), (10, 17), (11, 3), (12, 1)],
    },
    // A wider block holds a wider node, so the same payload maps through a far
    // shallower tree and needs fewer blocks — while the *bytes* rise, because
    // 64 KiB of payload occupies whole 4 KiB blocks whose content capacity it
    // cannot fill exactly. The transfer bound is sixteen of these blocks, so
    // the payload's seventeenth data block is a command of its own.
    Row {
        workload: Workload::OneCall,
        block_size: 4096,
        amplification: Some(156),
        cost: Cost {
            writes: 5,
            blocks: 25,
            distinct_blocks: 25,
            bytes: 102_400,
            barriers: 1,
        },
        runs: &[(1, 3), (6, 1), (16, 1)],
    },
    Row {
        workload: Workload::Chunked,
        block_size: 4096,
        amplification: Some(1_000),
        cost: Cost {
            writes: 64,
            blocks: 160,
            distinct_blocks: 136,
            bytes: 655_360,
            barriers: 16,
        },
        runs: &[(1, 32), (2, 16), (6, 16)],
    },
    // 34 bytes appended: one rewritten data block, and ten blocks of tree,
    // root, and ring slot to name it — four commands, the eight metadata blocks
    // among them adjacent and so gathered into one.
    Row {
        workload: Workload::SmallAppend,
        block_size: 512,
        amplification: Some(16_564),
        cost: Cost {
            writes: 4,
            blocks: 11,
            distinct_blocks: 11,
            bytes: 5_632,
            barriers: 1,
        },
        runs: &[(1, 3), (8, 1)],
    },
    // The floor: what an operation carrying no payload at all still costs.
    // The first mutation after mkfs also durably invalidates the clean map
    // stamp; metadata is one twelve-block run, followed by the slot pair.
    Row {
        workload: Workload::CreateFile,
        block_size: 512,
        amplification: None,
        cost: Cost {
            writes: 4,
            blocks: 15,
            distinct_blocks: 15,
            bytes: 7_680,
            barriers: 1,
        },
        runs: &[(1, 3), (12, 1)],
    },
];

impl Row {
    /// The row's recorded run-length histogram, in the form a measurement
    /// yields.
    fn run_lengths(&self) -> BTreeMap<u64, usize> {
        self.runs.iter().copied().collect()
    }
}

#[test]
fn the_write_path_costs_its_measured_baseline() {
    for row in BASELINE {
        let (cost, runs) =
            row.workload
                .measure(row.block_size, DEVICE_BYTES, Publish::PerOperation);
        assert_eq!(
            cost, row.cost,
            "{:?} at a {}-byte block size now costs {cost:?}",
            row.workload, row.block_size
        );
        assert_eq!(
            runs,
            row.run_lengths(),
            "{:?} at a {}-byte block size issued runs {runs:?}",
            row.workload,
            row.block_size
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

/// The drain hands the device one request per *physical run*, not one per
/// block: adjacent staged blocks — a mirrored metadata pair, a transaction's
/// consecutively allocated data blocks, the whole metadata working set of a
/// transaction that allocated it downward from the high end — leave together,
/// bounded only by the transfer window a controller moves on one DMA
/// descriptor.
///
/// The bytes are untouched; only the command count falls. That is the whole of
/// the claim: on a device priced per command, a 64 KiB write costs five
/// commands where it cost 158, and an empty-file create costs three where it
/// cost fourteen.
#[test]
fn the_drain_issues_one_request_per_physical_run() {
    for row in BASELINE {
        let (cost, runs) =
            row.workload
                .measure(row.block_size, DEVICE_BYTES, Publish::PerOperation);
        let bound = u64::try_from(RUN_BYTES).expect("a transfer bound fits a u64")
            / u64::from(row.block_size);
        for (&blocks, &commands) in &runs {
            assert!(
                blocks <= bound,
                "{:?} at a {}-byte block size issued {commands} commands of \
                 {blocks} blocks, past the {bound}-block transfer bound",
                row.workload,
                row.block_size
            );
        }
        // Nothing is coalesced away: the commands carry every block the
        // transaction wrote, and no more.
        assert_eq!(
            runs.iter()
                .map(|(blocks, count)| blocks * *count as u64)
                .sum::<u64>(),
            cost.blocks,
            "{:?} at a {}-byte block size lost blocks between the set and the \
             device",
            row.workload,
            row.block_size
        );
        assert!(
            u64::try_from(cost.writes).expect("a command count fits a u64") < cost.blocks,
            "{:?} at a {}-byte block size still costs one command per block: \
             {cost:?}",
            row.workload,
            row.block_size
        );
    }
}

/// A run reaching the very last block of the device is issued whole, and no run
/// ever reaches past it.
///
/// Metadata is allocated downward from the high end, so the first mirrored pair
/// a `mkfs` writes sits on the device's top two blocks and the run covering it
/// ends exactly at its end. That is the boundary a coalescer gathering past its
/// own evidence would overrun, and the fixture refuses an out-of-range request
/// rather than clamping it — a run is built only from addresses the set holds,
/// and every staged address is a block of the volume.
#[test]
fn a_run_reaching_the_end_of_the_device_is_written_whole() {
    for block_size in [512_u32, 4096] {
        // Not armed: the window under measurement is the format itself, which
        // is what puts a mirrored pair on the last two blocks.
        let (_fs, ledger) = volume(block_size, DEVICE_BYTES, Publish::PerOperation);
        let blocks = DEVICE_BYTES / u64::from(block_size);
        let held = ledger.borrow();
        let ends: Vec<u64> = held
            .commands
            .iter()
            .filter_map(|command| match *command {
                Command::Write { lba, blocks: run } => Some(lba + run),
                Command::Barrier => None,
            })
            .collect();
        assert_eq!(
            ends.iter().max().copied(),
            Some(blocks),
            "at a {block_size}-byte block size no run reached the device's last \
             block: {:?}",
            held.commands
        );
        assert!(
            ends.iter().all(|&end| end <= blocks),
            "at a {block_size}-byte block size a run ran past the {blocks}-block \
             device: {:?}",
            held.commands
        );
    }
}

/// A transaction writes each block it touches exactly once, however many times
/// it rewrote it.
///
/// Every stored data block ends in a B-tree insert that copy-on-writes the
/// covering leaf, and the transaction re-writes that leaf, the spine above it,
/// the inode, and the root once per data block. Holding the sealed bytes in the
/// dirty set and draining at the commit point collapses each of those to one
/// device write, so the 148-data-block case pays ten metadata writes — five
/// mirrored blocks — instead of the 598 the churn used to cost.
#[test]
fn a_transaction_writes_each_block_it_touches_exactly_once() {
    /// Data blocks 64 KiB of incompressible payload occupies at a 512-byte
    /// block size, cross-checked below against the driver's own accounting.
    const DATA_BLOCKS: u64 = 148;

    let (mut fs, ledger) = volume(512, DEVICE_BYTES, Publish::PerOperation);
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

    assert_eq!(
        cost.superseded(),
        0,
        "one transaction superseded {} of its own block writes: {cost:?}",
        cost.superseded()
    );
    assert!(
        cost.distinct_blocks > DATA_BLOCKS,
        "the payload's {DATA_BLOCKS} blocks must be mapped by metadata blocks \
         beyond them, not by nothing: {cost:?}"
    );
    assert!(
        cost.blocks - DATA_BLOCKS < DATA_BLOCKS / 8,
        "the metadata a transaction writes must be a fraction of its payload, \
         not a multiple: {} metadata blocks against {DATA_BLOCKS} data",
        cost.blocks - DATA_BLOCKS
    );
}

/// The durability ordering: every commit issues exactly one barrier, and the
/// only writes after it are the two copies of the superblock slot that
/// publishes the transaction — the companion first, so the copy a mount prefers
/// is the single write that makes the new state selectable.
///
/// This is the whole of the guarantee. A device with a volatile write cache may
/// reorder as it likes on either side of the barrier and still cannot make the
/// slot durable while a block beneath its root is not, which is the ordering
/// that turns one power cut into a whole-volume loss.
#[test]
fn a_commit_barriers_once_with_only_the_publishing_slot_after_it() {
    for row in BASELINE {
        let (mut fs, ledger) = volume(row.block_size, DEVICE_BYTES, Publish::PerOperation);
        row.workload.prepare(&mut fs);
        ledger.borrow_mut().arm();
        row.workload.run(&mut fs);
        let held = ledger.borrow();
        let barriers: Vec<usize> = held
            .commands
            .iter()
            .enumerate()
            .filter(|(_, command)| matches!(command, Command::Barrier))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            barriers.len(),
            row.cost.barriers,
            "{:?} at a {}-byte block size issued {} barriers",
            row.workload,
            row.block_size,
            barriers.len()
        );
        for at in barriers {
            let published = held.commands.get(at + 1..at + 3).unwrap_or_default();
            let mirror = match published {
                [Command::Write {
                    lba: companion,
                    blocks: 1,
                }, Command::Write {
                    lba: slot,
                    blocks: 1,
                }] => (*slot, *companion),
                other => panic!(
                    "{:?} at a {}-byte block size followed its barrier with \
                     {other:?}",
                    row.workload, row.block_size
                ),
            };
            assert_eq!(
                mirror.1,
                mirror.0 + 1,
                "the two writes after a barrier must be a mirror pair, \
                 companion first: {mirror:?}"
            );
        }
        // Every command from the last barrier on is accounted for by that
        // pair, so nothing a published root names was still unwritten when
        // the barrier was issued.
        let last = held
            .commands
            .iter()
            .rposition(|command| matches!(command, Command::Barrier))
            .expect("a commit always barriers");
        assert_eq!(
            held.commands.len() - last - 1,
            2,
            "{:?} at a {}-byte block size wrote {} blocks after its final \
             barrier",
            row.workload,
            row.block_size,
            held.commands.len() - last - 1
        );
    }
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
        let (one_call, _) =
            Workload::OneCall.measure(block_size, DEVICE_BYTES, Publish::PerOperation);
        let (chunked, _) =
            Workload::Chunked.measure(block_size, DEVICE_BYTES, Publish::PerOperation);
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
/// Directories the refusal fixture fills the volume's trees with, in the two
/// sizes the scaling assertion compares — sixteen times the metadata, so a cost
/// that walks it shows as a multiple rather than as noise.
const REFUSAL_FIXTURE_SIZES: [usize; 2] = [64, 1024];

/// Device size the refusal fixture is measured on: room for the larger of those
/// directory counts and the trees over them, where the baseline's eight
/// mebibytes has none. Nothing materialises it — the fixture stores only the
/// blocks written.
const REFUSAL_DEVICE_BYTES: u64 = 256 << 20;

/// A volume holding `dirs` directories off the root, plus one nearly-empty
/// working directory, and the ledger recording what its driver issues.
///
/// The metadata lives away from the directory the measured operations act on,
/// so what varies between fixture sizes is the volume's total metadata and not
/// the size of the directory being written.
fn refusal_fixture(dirs: usize) -> (ARXFS<LedgerBlock>, NodeId, Rc<RefCell<Ledger>>) {
    let (mut fs, ledger) = volume(4096, REFUSAL_DEVICE_BYTES, Publish::PerOperation);
    let root = fs.root();
    for index in 0..dirs {
        let name = format!("d{index:05}");
        fs.create(root, name.as_bytes(), NodeKind::Directory)
            .expect("fixture directory");
    }
    let work = fs
        .create(root, b"work", NodeKind::Directory)
        .expect("working directory");
    (fs, work, ledger)
}

/// An operation refused for an ordinary reason — a name already taken — leaves
/// the operation after it costing exactly what it would have cost had the
/// refusal never happened, and the refusal itself reads no more than the
/// operation it refused.
///
/// Rolling back an unpublished transaction is that transaction's own
/// bookkeeping run backwards, so neither figure may grow with the volume's
/// metadata. A rollback that instead discarded the allocation map made every
/// refusal provoke a walk of every tree on the volume: unbounded read
/// amplification from a call that changes nothing, reachable by any caller who
/// may create a name, and worse the fuller the volume.
#[test]
fn a_refused_operation_leaves_the_next_one_costing_what_it_always_did() {
    for dirs in REFUSAL_FIXTURE_SIZES {
        let (mut fs, work, ledger) = refusal_fixture(dirs);
        let taken = b"already-here";
        fs.create(work, taken, NodeKind::RegularFile)
            .expect("the name under test");

        ledger.borrow_mut().arm();
        fs.create(work, b"clean", NodeKind::RegularFile)
            .expect("create with no refusal before it");
        let clean = ledger.borrow().read_blocks;

        ledger.borrow_mut().arm();
        assert_eq!(
            fs.create(work, taken, NodeKind::RegularFile),
            Err(DriverError::AlreadyExists),
            "the fixture's taken name must be refused"
        );
        let refusal = ledger.borrow().read_blocks;

        ledger.borrow_mut().arm();
        fs.create(work, b"after", NodeKind::RegularFile)
            .expect("create after a refusal");
        let after = ledger.borrow().read_blocks;

        assert_eq!(
            after, clean,
            "over {dirs} directories, a create read {after} blocks after a \
             refused one against {clean} with none before it"
        );
        assert!(
            refusal <= clean,
            "over {dirs} directories, refusing a create read {refusal} blocks \
             where performing one reads {clean}"
        );
    }
}

#[test]
fn the_cost_of_a_write_does_not_grow_with_the_volume() {
    for workload in [
        Workload::OneCall,
        Workload::Chunked,
        Workload::SmallAppend,
        Workload::CreateFile,
    ] {
        let (small, small_runs) = workload.measure(4096, DEVICE_BYTES, Publish::PerOperation);
        let (huge, huge_runs) = workload.measure(4096, FLOOR_DEVICE_BYTES, Publish::PerOperation);
        assert_eq!(
            huge, small,
            "{workload:?} costs {huge:?} on a {FLOOR_DEVICE_BYTES}-byte volume \
             against {small:?} on a {DEVICE_BYTES}-byte one"
        );
        assert_eq!(huge_runs, small_runs, "{workload:?} run lengths moved");
    }
}

/// What a *batched* window costs, where the operations inside it join one
/// transaction rather than publishing one each.
///
/// The fixture is published before the window is armed and handed on at its
/// end, so each row prices the workload's own writes plus the single commit
/// that publishes them — including the map invalidation the published fixture
/// makes the window pay, which is why a row here costs one command more than
/// its per-operation counterpart in [`BASELINE`].
const BATCHED: &[Row] = &[
    Row {
        workload: Workload::OneCall,
        block_size: 512,
        amplification: Some(124),
        cost: Cost {
            writes: 6,
            blocks: 159,
            distinct_blocks: 159,
            bytes: 81_408,
            barriers: 1,
        },
        runs: &[(1, 3), (8, 1), (20, 1), (128, 1)],
    },
    // Sixteen calls, one transaction: the same blocks, the same bytes, and
    // nothing superseded. The one extra command is the metadata run splitting
    // in two, because the chunked path takes its tree blocks in a different
    // order.
    Row {
        workload: Workload::Chunked,
        block_size: 512,
        amplification: Some(124),
        cost: Cost {
            writes: 7,
            blocks: 159,
            distinct_blocks: 159,
            bytes: 81_408,
            barriers: 1,
        },
        runs: &[(1, 3), (4, 2), (20, 1), (128, 1)],
    },
    Row {
        workload: Workload::OneCall,
        block_size: 4096,
        amplification: Some(162),
        cost: Cost {
            writes: 6,
            blocks: 26,
            distinct_blocks: 26,
            bytes: 106_496,
            barriers: 1,
        },
        runs: &[(1, 4), (6, 1), (16, 1)],
    },
    Row {
        workload: Workload::Chunked,
        block_size: 4096,
        amplification: Some(162),
        cost: Cost {
            writes: 7,
            blocks: 26,
            distinct_blocks: 26,
            bytes: 106_496,
            barriers: 1,
        },
        runs: &[(1, 4), (2, 1), (4, 1), (16, 1)],
    },
];

/// Chunking a payload into many calls costs what one call costs, once the
/// calls join one transaction.
///
/// This is what the commit scheduler is for. Per operation, the same 64 KiB
/// in sixteen calls costs sixteen transaction roots, sixteen ring slots,
/// sixteen barriers, and sixteen rewrites of the spine — 64 commands against
/// five, and 24 superseded blocks. Joined, it is the same blocks and the same
/// bytes as the single call, with nothing superseded and one barrier.
#[test]
fn a_batched_window_costs_what_the_same_bytes_cost_in_one_call() {
    for row in BATCHED {
        let (cost, runs) = row
            .workload
            .measure(row.block_size, DEVICE_BYTES, Publish::Batched);
        assert_eq!(
            cost, row.cost,
            "{:?} batched at a {}-byte block size now costs {cost:?}",
            row.workload, row.block_size
        );
        assert_eq!(
            runs,
            row.run_lengths(),
            "{:?} batched at a {}-byte block size issued runs {runs:?}",
            row.workload,
            row.block_size
        );
        assert_eq!(
            cost.amplification_hundredths(row.workload.payload_bytes()),
            row.amplification
        );
        assert_eq!(
            cost.superseded(),
            0,
            "a joined transaction writes each block it touches exactly once"
        );
    }

    for block_size in [512_u32, 4096] {
        let (one_call, _) = Workload::OneCall.measure(block_size, DEVICE_BYTES, Publish::Batched);
        let (chunked, _) = Workload::Chunked.measure(block_size, DEVICE_BYTES, Publish::Batched);
        assert_eq!(
            (chunked.blocks, chunked.bytes, chunked.barriers),
            (one_call.blocks, one_call.bytes, one_call.barriers),
            "at a {block_size}-byte block size the chunked window must put the \
             same bytes on the device, behind one barrier, as the single call"
        );
        assert!(
            chunked.writes <= one_call.writes + 1,
            "at a {block_size}-byte block size, {} chunked commands against {} \
             for one call",
            chunked.writes,
            one_call.writes
        );
    }
}

// ---------------------------------------------------------------------------
// The bound: what the cache is allowed to pin, and what happens when a writer
// outruns the device (`plans/ARXFS-WRITEBACK.md` §6).
// ---------------------------------------------------------------------------

/// A volume bounded as the host bounds one, plus the pinned ledger the host
/// would register and the ledger recording what the device is issued.
///
/// `backing_bytes` stands in for the RAM the boot path discovered, so a test
/// picks the ceiling by picking a machine size — the same derivation a mount
/// uses, never a bound spelled directly.
fn bounded_volume(
    block_size: u32,
    device_bytes: u64,
    backing_bytes: usize,
    band: PressureBand,
) -> (
    ARXFS<LedgerBlock>,
    Arc<PinnedAccounting>,
    Rc<RefCell<Ledger>>,
) {
    let (fs, ledger) = volume(block_size, device_bytes, Publish::Batched);
    let pinned = Arc::new(PinnedAccounting::new());
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(band);
    let fs = fs
        .with_writeback_bound(backing_bytes, gauge, Arc::clone(&pinned))
        .expect("the test machine bounds the volume");
    (fs, pinned, ledger)
}

/// Machine size whose per-volume share is a handful of transfer windows, so a
/// payload larger than the ceiling is reachable without a huge test.
const SMALL_MACHINE_BYTES: usize = 16 * (4 * RUN_BYTES);

/// Machine size whose per-volume share is exactly one transfer window — the
/// smallest a volume may be mounted on at all.
const FLOOR_MACHINE_BYTES: usize = 16 * RUN_BYTES;

/// Payload that comfortably outruns [`SMALL_MACHINE_BYTES`]'s ceiling.
const OUTRUN_BYTES: usize = 24 * RUN_BYTES;

/// Write `data` whole, looping over the short counts the bound produces, and
/// report how many calls it took.
///
/// The peak the set pinned is read from the ledger's own high-water mark
/// rather than sampled between calls: a call that fills the ceiling publishes
/// before it returns, so anything measured from outside would see the residue
/// and never the peak.
fn write_whole_bounded(
    fs: &mut ARXFS<LedgerBlock>,
    dir: NodeId,
    name: &[u8],
    data: &[u8],
) -> usize {
    let mut done = 0usize;
    let mut calls = 0usize;
    while done < data.len() {
        let at = u64::try_from(done).expect("an offset fits a u64");
        let written = fs
            .write_at(dir, name, at, &data[done..])
            .expect("a bounded write still stores bytes");
        assert!(written > 0, "the bound must never stall a writer");
        done += written;
        calls += 1;
    }
    calls
}

/// A writer that outruns the device is throttled by forced commits, not by
/// growth: the dirty set never exceeds the ceiling by more than the one record
/// the write was in the middle of, the transaction is published repeatedly to
/// make room, and every byte still lands.
#[test]
fn a_writer_that_outruns_the_device_is_throttled_rather_than_allowed_to_grow() {
    let (mut fs, pinned, ledger) =
        bounded_volume(512, DEVICE_BYTES, SMALL_MACHINE_BYTES, PressureBand::Normal);
    let root = fs.root();
    fs.create(root, FILE, NodeKind::RegularFile)
        .expect("create");
    fs.flush().expect("start from a published volume");

    let body = payload(OUTRUN_BYTES);
    ledger.borrow_mut().arm();
    let calls = write_whole_bounded(&mut fs, root, FILE, &body);
    let peak = pinned.peak_bytes();
    let ceiling = SMALL_MACHINE_BYTES / 16;

    assert!(calls > 1, "the bound cut the write into more than one call");
    assert!(
        peak <= ceiling + RUN_BYTES,
        "the set pinned {peak} bytes against a {ceiling}-byte ceiling: \
         back-pressure must bound growth, not merely slow it"
    );
    assert!(
        peak > 0,
        "a write that pins nothing is not measuring the cache"
    );
    assert!(
        pinned.released() > 1,
        "a payload {OUTRUN_BYTES} bytes wide under a {ceiling}-byte ceiling \
         must have been written out more than once"
    );
    assert!(
        pinned.refusals() > 0,
        "the bound cut the write short at least once"
    );
    assert!(
        Cost::of(&ledger.borrow(), 512).barriers > 1,
        "each forced commit carries its own durability barrier"
    );

    // Correctness is the point: throttling must not lose or reorder a byte.
    fs.flush().expect("publish the tail");
    let file = fs.lookup(root, FILE).expect("the file");
    let mut read = vec![0u8; OUTRUN_BYTES];
    assert_eq!(fs.read_at(file, 0, &mut read), Ok(OUTRUN_BYTES));
    assert_eq!(read, body, "the throttled write must be byte-exact");
    assert_eq!(pinned.bytes(), 0, "a published volume pins nothing");
}

/// The driver's own whole-write path resumes across the short counts the bound
/// produces, so a caller that must store an indivisible value never sees one.
#[test]
fn the_whole_write_path_resumes_across_a_bounded_short_write() {
    let (mut fs, _, _) =
        bounded_volume(512, DEVICE_BYTES, SMALL_MACHINE_BYTES, PressureBand::Normal);
    let root = fs.root();
    let body = payload(OUTRUN_BYTES);
    tairix_drv_fs_arxfs::plant_nested_file(&mut fs, root, &[b"deep", b"planted"], &body)
        .expect("a planted payload is stored whole across short writes");
    fs.flush().expect("publish");

    let dir = fs.lookup(root, b"deep").expect("the directory");
    let file = fs.lookup(dir, b"planted").expect("the planted file");
    let mut read = vec![0u8; OUTRUN_BYTES];
    assert_eq!(fs.read_at(file, 0, &mut read), Ok(OUTRUN_BYTES));
    assert_eq!(read, body);
}

/// Tightening memory lowers the ceiling, so the same payload is written out
/// more often — the response to pressure is publish sooner, never hold more.
#[test]
fn rising_pressure_publishes_more_often_for_the_same_payload() {
    let body = payload(OUTRUN_BYTES);
    let mut released = Vec::new();
    for band in [PressureBand::Normal, PressureBand::Critical] {
        let (mut fs, pinned, _) = bounded_volume(512, DEVICE_BYTES, SMALL_MACHINE_BYTES, band);
        let root = fs.root();
        fs.create(root, FILE, NodeKind::RegularFile)
            .expect("create");
        write_whole_bounded(&mut fs, root, FILE, &body);
        fs.flush().expect("publish the tail");
        released.push((band, pinned.released(), pinned.peak_bytes()));
    }
    let (_, calm_released, calm_peak) = released[0];
    let (_, tight_released, tight_peak) = released[1];
    assert!(
        tight_released > calm_released,
        "a critical machine published {tight_released} times against \
         {calm_released} on an unpressured one"
    );
    assert!(
        tight_peak < calm_peak,
        "a critical machine pinned {tight_peak} bytes against {calm_peak} on \
         an unpressured one"
    );
}

/// The forward-progress floor: on the smallest machine a volume may be
/// mounted on — one transfer window per volume — a write far larger than the
/// whole ceiling still completes, byte-exact, without stalling.
#[test]
fn the_floor_lets_a_transaction_complete_on_the_smallest_supported_machine() {
    let (mut fs, pinned, _) = bounded_volume(
        512,
        DEVICE_BYTES,
        FLOOR_MACHINE_BYTES,
        PressureBand::Critical,
    );
    let root = fs.root();
    fs.create(root, FILE, NodeKind::RegularFile)
        .expect("create");
    let body = payload(OUTRUN_BYTES);
    write_whole_bounded(&mut fs, root, FILE, &body);
    fs.flush().expect("publish");
    let peak = pinned.peak_bytes();
    assert!(
        peak <= 2 * RUN_BYTES,
        "the floor machine pinned {peak} bytes"
    );
    assert!(pinned.released() > 1);
    let file = fs.lookup(root, FILE).expect("the file");
    let mut read = vec![0u8; OUTRUN_BYTES];
    assert_eq!(fs.read_at(file, 0, &mut read), Ok(OUTRUN_BYTES));
    assert_eq!(read, body);
}

/// The combined floor the charter binds every storage design to: the smallest
/// supported machine serving a hundred tebibytes, writing more than its whole
/// ceiling. Resident dirty bytes stay bounded, the bytes are exact, and
/// nothing panics or spins.
#[test]
fn a_small_machine_bounds_its_dirty_set_on_a_hundred_tebibyte_volume() {
    for block_size in [512_u32, 4096] {
        let (mut fs, pinned, _) = bounded_volume(
            block_size,
            FLOOR_DEVICE_BYTES,
            FLOOR_MACHINE_BYTES,
            PressureBand::Moderate,
        );
        let root = fs.root();
        fs.create(root, FILE, NodeKind::RegularFile)
            .expect("create");
        let body = payload(OUTRUN_BYTES);
        write_whole_bounded(&mut fs, root, FILE, &body);
        fs.flush().expect("publish");
        let peak = pinned.peak_bytes();
        assert!(
            peak <= 2 * RUN_BYTES,
            "a {block_size}-byte volume of {FLOOR_DEVICE_BYTES} bytes pinned \
             {peak} bytes on the floor machine"
        );
        assert_eq!(pinned.bytes(), 0);
        let file = fs.lookup(root, FILE).expect("the file");
        let mut read = vec![0u8; OUTRUN_BYTES];
        assert_eq!(fs.read_at(file, 0, &mut read), Ok(OUTRUN_BYTES));
        assert_eq!(read, body);
    }
}

/// The ledger reports the pinned bytes while they are pinned, and reports them
/// gone once the transaction is published.
#[test]
fn the_ledger_reports_a_volumes_unwritten_bytes() {
    let (mut fs, pinned, _) =
        bounded_volume(512, DEVICE_BYTES, SMALL_MACHINE_BYTES, PressureBand::Normal);
    let root = fs.root();
    fs.create(root, FILE, NodeKind::RegularFile)
        .expect("create");
    assert!(
        pinned.bytes() > 0,
        "an open transaction's blocks are pinned and reported"
    );
    assert_eq!(pinned.bytes(), fs.writeback_pinned_bytes());
    let row = PinnedLedger::new(
        "arxfs.writeback",
        ReclaimOwner::FilesystemVolume { volume: 9 },
        Arc::clone(&pinned),
    )
    .to_record()
    .expect("the row encodes");
    assert_eq!(row.payload_bytes, pinned.bytes() as u64);
    assert!(row.entries > 0);

    fs.flush().expect("publish");
    assert_eq!(pinned.bytes(), 0);
    assert_eq!(fs.writeback_pinned_bytes(), 0);
}

/// A read-only mount can never stage a block, so it pins nothing and costs a
/// bounded machine nothing.
#[test]
fn a_read_only_mount_pins_nothing() {
    let (fs, _) = volume(512, DEVICE_BYTES, Publish::Batched);
    let root = fs.root();
    let mut fs = fs;
    fs.create(root, FILE, NodeKind::RegularFile)
        .expect("create");
    fs.flush().expect("publish");
    let device = fs.into_block().expect("hand the volume on");

    let pinned = Arc::new(PinnedAccounting::new());
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(PressureBand::Normal);
    let mut fs = ARXFS::open_read_only(device, &TEST_KEY)
        .expect("the volume mounts read-only")
        .with_writeback_bound(SMALL_MACHINE_BYTES, gauge, Arc::clone(&pinned))
        .expect("a read-only mount is bounded like any other");
    assert_eq!(
        fs.write_at(root, FILE, 0, b"x"),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(fs.writeback_pinned_bytes(), 0);
    assert_eq!((pinned.bytes(), pinned.entries()), (0, 0));
}

// ---------------------------------------------------------------------------
// The combined floor: several 100 TiB volumes mounted and writing at once on
// one small machine — the worst case every storage design is held to.
// ---------------------------------------------------------------------------

/// Volumes the combined-floor case mounts, writes to, and holds dirty at
/// once. Enough that a per-volume ceiling would let them pin a multiple of
/// the machine.
const FLOOR_VOLUMES: usize = 4;

/// A machine whose free reading falls as its mounted volumes pin bytes.
///
/// This is the kernel's own gauge source in miniature: a staged block is a
/// real allocation, so the frame allocator loses the free frame while the
/// block is pinned, and every volume decides its ceiling against that one
/// shared reading.
struct Machine {
    total: usize,
    share: &'static PinnedShare,
}

impl FreeMemorySource for Machine {
    fn free_bytes(&self) -> usize {
        self.total.saturating_sub(self.share.bytes())
    }

    fn total_bytes(&self) -> usize {
        self.total
    }
}

/// Several 100 TiB volumes, mounted and writing at once, share one machine's
/// bounded dirty total rather than each taking its own slab of it: the bytes
/// pinned across every volume *at once* stay inside the machine's one derived
/// ceiling, every volume's payload lands byte-exact, and nothing panics or
/// spins.
///
/// The volumes advance a slice at a time in turn, so each is holding what it
/// has staged while the others decide what they may stage — which is the state
/// a per-volume ceiling gets wrong and the state a machine actually has to
/// survive. Both machine sizes matter: the larger divides into shares well
/// above one device transfer, so the shared ceiling is what bounds the total;
/// the smaller divides into shares below one, so every volume takes the
/// forward-progress floor and the total is the floor's — the most a machine
/// can be asked to give up if each of its volumes is to be able to finish a
/// transaction at all.
#[test]
fn several_hundred_tebibyte_volumes_share_one_machines_dirty_total() {
    for machine_bytes in [16 * SMALL_MACHINE_BYTES, SMALL_MACHINE_BYTES] {
        let share: &'static PinnedShare = Box::leak(Box::new(PinnedShare::new()));
        let machine: &'static Machine = Box::leak(Box::new(Machine {
            total: machine_bytes,
            share,
        }));
        let gauge: &'static MemoryPressure = Box::leak(Box::new(MemoryPressure::over(machine)));

        let slots: Vec<Arc<PinnedAccounting>> = (0..FLOOR_VOLUMES)
            .map(|_| Arc::new(PinnedAccounting::within(share)))
            .collect();
        let mut mounted = Vec::new();
        for slot in &slots {
            let (fs, _) = volume(512, FLOOR_DEVICE_BYTES, Publish::Batched);
            let mut fs = fs
                .with_writeback_bound(machine_bytes, gauge, Arc::clone(slot))
                .expect("the machine bounds every volume it mounts");
            let root = fs.root();
            fs.create(root, FILE, NodeKind::RegularFile)
                .expect("create");
            mounted.push(fs);
        }

        let body = payload(OUTRUN_BYTES);
        let mut done = [0usize; FLOOR_VOLUMES];
        while done.iter().any(|written| *written < body.len()) {
            for (index, fs) in mounted.iter_mut().enumerate() {
                let from = done[index];
                if from >= body.len() {
                    continue;
                }
                let slice = &body[from..body.len().min(from + CHUNK_BYTES)];
                let root = fs.root();
                let at = u64::try_from(from).expect("an offset fits a u64");
                let written = fs
                    .write_at(root, FILE, at, slice)
                    .expect("a bounded write still stores bytes");
                assert!(written > 0, "the bound must never stall a writer");
                done[index] += written;
            }
        }

        let peak = share.peak_bytes();
        let ceiling = CacheBudget::from_backing(machine_bytes).hard();
        // One record may be in flight per volume above its share: the write
        // path always stores at least one before the bound can cut it short.
        let each = (ceiling / FLOOR_VOLUMES).max(RUN_BYTES) + RUN_BYTES;
        assert!(
            peak <= FLOOR_VOLUMES * each,
            "{FLOOR_VOLUMES} volumes pinned {peak} bytes at once against a \
             {ceiling}-byte machine-wide ceiling on a {machine_bytes}-byte \
             machine: a machine's volumes must share one bounded total, not \
             take a slab each"
        );
        assert!(
            slots.iter().all(|slot| slot.released() > 0),
            "the shared bound bit on every volume and forced it to write out"
        );

        for (index, fs) in mounted.iter_mut().enumerate() {
            fs.flush().expect("publish the tail");
            let file = fs.lookup(fs.root(), FILE).expect("the file");
            let mut read = vec![0u8; body.len()];
            assert_eq!(fs.read_at(file, 0, &mut read), Ok(body.len()));
            assert_eq!(read, body, "volume {index} lost or reordered a byte");
        }
        assert_eq!(share.bytes(), 0, "a published volume pins nothing");
    }
}
