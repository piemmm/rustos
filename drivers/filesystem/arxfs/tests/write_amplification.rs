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
//! Two of the properties asserted here are measurements of the present, not
//! endorsements of it, and each is the acceptance hook of a named later stage:
//! sixteen calls writing one payload still cost far more than one call writing
//! it (the commit scheduler converges them), and every write still carries
//! exactly one block, because the drain issues one command per staged block
//! while the read path already gathers runs (the coalescer weights that
//! histogram toward the staging window). The rest is the write-back cache's
//! contract, held here: a transaction writes each block it touches once, and
//! every commit issues exactly one barrier with nothing but the two copies of
//! its publishing superblock slot after it.

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

/// What the write path costs the device: one command per block, one barrier per
/// commit, and each block a transaction touches written exactly once.
///
/// A change to any figure here is a real change in what a write costs a device,
/// so the stage that improves one updates the row it improved, and a change
/// that did not mean to touch the write path is told that it did.
const BASELINE: &[Row] = &[
    // 64 KiB in one call. 148 of the 158 blocks are the payload's own data
    // records (443 content bytes fit a 512-byte block); the other ten are five
    // mirrored metadata blocks — the extent and inode trees, the transaction
    // root, and the ring slot — each written once however many times the
    // transaction rewrote it.
    Row {
        workload: Workload::OneCall,
        block_size: 512,
        amplification: Some(123),
        cost: Cost {
            writes: 158,
            blocks: 158,
            distinct_blocks: 158,
            bytes: 80_896,
            barriers: 1,
        },
    },
    // The same bytes in sixteen calls: sixteen transactions, each republishing
    // the spine, a fresh root, and a ring slot, and each rewriting the
    // partially filled block the previous call left. The churn that survives is
    // therefore *between* transactions, which is what the commit scheduler
    // closes; within each one, every block is still written once.
    Row {
        workload: Workload::Chunked,
        block_size: 512,
        amplification: Some(261),
        cost: Cost {
            writes: 335,
            blocks: 335,
            distinct_blocks: 311,
            bytes: 171_520,
            barriers: 16,
        },
    },
    // A wider block holds a wider node, so the same payload maps through a far
    // shallower tree and needs fewer commands — while the *bytes* rise, because
    // 64 KiB of payload occupies whole 4 KiB blocks whose content capacity it
    // cannot fill exactly.
    Row {
        workload: Workload::OneCall,
        block_size: 4096,
        amplification: Some(156),
        cost: Cost {
            writes: 25,
            blocks: 25,
            distinct_blocks: 25,
            bytes: 102_400,
            barriers: 1,
        },
    },
    Row {
        workload: Workload::Chunked,
        block_size: 4096,
        amplification: Some(1_000),
        cost: Cost {
            writes: 160,
            blocks: 160,
            distinct_blocks: 136,
            bytes: 655_360,
            barriers: 16,
        },
    },
    // 34 bytes appended: one rewritten data block, and ten blocks of tree,
    // root, and ring slot to name it.
    Row {
        workload: Workload::SmallAppend,
        block_size: 512,
        amplification: Some(16_564),
        cost: Cost {
            writes: 11,
            blocks: 11,
            distinct_blocks: 11,
            bytes: 5_632,
            barriers: 1,
        },
    },
    // The floor: what an operation carrying no payload at all still costs.
    Row {
        workload: Workload::CreateFile,
        block_size: 512,
        amplification: None,
        cost: Cost {
            writes: 14,
            blocks: 14,
            distinct_blocks: 14,
            bytes: 7_168,
            barriers: 1,
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
        let (mut fs, ledger) = volume(row.block_size, DEVICE_BYTES);
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
