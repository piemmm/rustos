//! Host tests for the transport-neutral SCSI command layer over a
//! scripted [`ScsiTransport`] (`plans/DEVICES.md` D5): per-set CDB
//! spelling, write-protect via the set's MODE SENSE form, flush
//! semantics, the bounded ready drain, in-band autosense, and the
//! [`LunBlock`] validation/chunking/scrub behaviour.

use super::*;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// One scripted answer to an executed command.
struct Step {
    /// The device's verdict.
    passed: bool,
    /// Bytes delivered into an IN data phase (short if fewer than asked).
    data: Vec<u8>,
    /// In-band sense to stash for [`ScsiTransport::take_sense`] after this
    /// command (an autosense transport).
    sense: Option<Sense>,
}

/// A scripted command executor: records every `(lun, cdb, phase length)`
/// and answers from its step queue.
struct ScriptedExecutor {
    steps: VecDeque<Step>,
    /// Recorded executions: LUN, CDB bytes, data-phase length, is-IN.
    calls: Vec<(u8, Vec<u8>, usize, bool)>,
    /// LUN count the transport reports.
    luns: u8,
    /// The pending autosense capture.
    sense: Option<Sense>,
    scrubs: usize,
}

impl ScriptedExecutor {
    fn new() -> Self {
        Self {
            steps: VecDeque::new(),
            calls: Vec::new(),
            luns: 1,
            sense: None,
            scrubs: 0,
        }
    }

    fn pass_with(&mut self, data: Vec<u8>) {
        self.steps.push_back(Step {
            passed: true,
            data,
            sense: None,
        });
    }

    fn pass(&mut self) {
        self.pass_with(Vec::new());
    }

    fn fail(&mut self) {
        self.steps.push_back(Step {
            passed: false,
            data: Vec::new(),
            sense: None,
        });
    }

    fn fail_with_sense(&mut self, sense: Sense) {
        self.steps.push_back(Step {
            passed: false,
            data: Vec::new(),
            sense: Some(sense),
        });
    }
}

impl ScsiTransport for ScriptedExecutor {
    fn execute(
        &mut self,
        lun: u8,
        cdb: &[u8],
        data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno> {
        // A new execution invalidates any stale capture (the trait's
        // documented contract).
        self.sense = None;
        let is_in = data.is_in();
        let len = data.len();
        self.calls.push((lun, cdb.to_vec(), len, is_in));
        let step = self.steps.pop_front().ok_or(Errno::NotFound)?;
        let mut transferred = 0;
        if let DataPhase::In(buf) = data {
            let n = step.data.len().min(buf.len());
            buf[..n].copy_from_slice(&step.data[..n]);
            transferred = n;
        } else if let DataPhase::Out(buf) = data {
            transferred = buf.len();
        }
        self.sense = step.sense;
        Ok(CommandOutcome {
            passed: step.passed,
            transferred,
        })
    }

    fn lun_count(&mut self) -> Result<u8, Errno> {
        Ok(self.luns)
    }

    fn take_sense(&mut self) -> Option<Sense> {
        self.sense.take()
    }

    fn scrub(&mut self) {
        self.scrubs += 1;
    }
}

fn transparent(executor: ScriptedExecutor) -> ScsiDevice<ScriptedExecutor> {
    ScsiDevice::new(executor, CommandSet::Transparent)
}

fn ufi(executor: ScriptedExecutor) -> ScsiDevice<ScriptedExecutor> {
    ScsiDevice::new(executor, CommandSet::Ufi)
}

#[test]
fn ufi_pads_every_command_block_to_twelve_bytes() {
    let mut executor = ScriptedExecutor::new();
    executor.pass();
    let mut scsi = ufi(executor);
    assert_eq!(scsi.test_unit_ready(0), Ok(true));
    let (_, cdb, _, _) = &scsi.transport().calls[0];
    assert_eq!(cdb.len(), 12);
    assert!(cdb.iter().all(|&b| b == 0));
}

#[test]
fn transparent_sends_the_native_command_length() {
    let mut executor = ScriptedExecutor::new();
    executor.pass();
    let mut scsi = transparent(executor);
    assert_eq!(scsi.test_unit_ready(0), Ok(true));
    let (_, cdb, _, _) = &scsi.transport().calls[0];
    assert_eq!(cdb.len(), 6);
}

#[test]
fn write_protect_uses_the_sets_own_mode_sense_form() {
    // Transparent: MODE SENSE(6), WP at header byte 2.
    let mut executor = ScriptedExecutor::new();
    executor.pass_with(vec![0x03, 0x00, 0x80, 0x00]);
    let mut scsi = transparent(executor);
    assert_eq!(scsi.write_protected(0), Ok(true));
    let (_, cdb, _, _) = &scsi.transport().calls[0];
    assert_eq!(cdb[0], 0x1A);

    // UFI: MODE SENSE(10), WP at header byte 3.
    let mut executor = ScriptedExecutor::new();
    executor.pass_with(vec![0x00, 0x46, 0x00, 0x80, 0, 0, 0, 0]);
    let mut scsi = ufi(executor);
    assert_eq!(scsi.write_protected(0), Ok(true));
    let (_, cdb, _, _) = &scsi.transport().calls[0];
    assert_eq!(cdb[0], 0x5A);

    // A device that fails MODE SENSE is reported write-enabled.
    let mut executor = ScriptedExecutor::new();
    executor.fail();
    let mut scsi = transparent(executor);
    assert_eq!(scsi.write_protected(0), Ok(false));
}

#[test]
fn ufi_flush_is_a_no_op_and_transparent_flush_hits_the_wire() {
    // UFI has no SYNCHRONIZE CACHE: the flush succeeds with no command.
    let executor = ScriptedExecutor::new();
    let mut scsi = ufi(executor);
    assert_eq!(scsi.synchronize_cache(0), Ok(()));
    assert!(scsi.transport().calls.is_empty());

    // Transparent issues SYNCHRONIZE CACHE(10).
    let mut executor = ScriptedExecutor::new();
    executor.pass();
    let mut scsi = transparent(executor);
    assert_eq!(scsi.synchronize_cache(0), Ok(()));
    let (_, cdb, _, _) = &scsi.transport().calls[0];
    assert_eq!(cdb[0], 0x35);
}

#[test]
fn a_flush_refused_as_illegal_request_means_no_cache() {
    let mut executor = ScriptedExecutor::new();
    executor.fail_with_sense(Sense {
        key: 0x05, // ILLEGAL REQUEST
        asc: 0x20,
        ascq: 0,
    });
    let mut scsi = transparent(executor);
    assert_eq!(scsi.synchronize_cache(0), Ok(()));
    // The autosense answered the question; no REQUEST SENSE round trip.
    assert_eq!(scsi.transport().calls.len(), 1);
}

#[test]
fn autosense_maps_a_failed_read_without_request_sense() {
    let mut executor = ScriptedExecutor::new();
    executor.fail_with_sense(Sense {
        key: 0x07, // DATA PROTECT
        asc: 0x27,
        ascq: 0,
    });
    let mut scsi = transparent(executor);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::PermissionDenied));
    // Exactly one execution: the sense came in-band.
    assert_eq!(scsi.transport().calls.len(), 1);
}

#[test]
fn a_failed_read_without_autosense_asks_the_device() {
    let mut executor = ScriptedExecutor::new();
    executor.fail();
    // The REQUEST SENSE round trip: fixed format, NOT READY.
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x02; // NOT READY
    sense[12] = 0x3A;
    executor.pass_with(sense);
    let mut scsi = transparent(executor);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::WouldBlock));
    assert_eq!(scsi.transport().calls.len(), 2);
    let (_, cdb, _, _) = &scsi.transport().calls[1];
    assert_eq!(cdb[0], 0x03); // REQUEST SENSE
}

#[test]
fn ready_drain_consumes_sense_per_failed_attempt_and_is_bounded() {
    // Two not-ready answers, then ready.
    let mut executor = ScriptedExecutor::new();
    for _ in 0..2 {
        executor.fail();
        let mut sense = vec![0u8; 18];
        sense[0] = 0x70;
        sense[2] = 0x06; // UNIT ATTENTION
        executor.pass_with(sense);
    }
    executor.pass();
    let mut scsi = transparent(executor);
    assert_eq!(scsi.ready_after_drain(0, 8), Ok(true));
    assert_eq!(scsi.transport().calls.len(), 5);

    // A unit that never becomes ready is reported so after exactly
    // `attempts` round trips (each with its sense), never spun on.
    let mut executor = ScriptedExecutor::new();
    for _ in 0..3 {
        executor.fail_with_sense(Sense {
            key: 0x02,
            asc: 0x04,
            ascq: 0x01,
        });
    }
    let mut scsi = transparent(executor);
    assert_eq!(scsi.ready_after_drain(0, 3), Ok(false));
    assert_eq!(scsi.transport().calls.len(), 3);
}

#[test]
fn inquiry_accepts_a_short_but_sufficient_answer() {
    let mut executor = ScriptedExecutor::new();
    executor.pass_with(vec![0x00, 0x80, 0x05, 0x02, 0x1F]);
    let mut scsi = transparent(executor);
    assert_eq!(
        scsi.inquiry(0),
        Ok(Inquiry {
            device_type: DEVICE_TYPE_DIRECT_ACCESS,
            removable: true,
        })
    );
}

#[test]
fn read_capacity_validates_the_block_size() {
    let mut executor = ScriptedExecutor::new();
    let mut payload = Vec::new();
    payload.extend_from_slice(&1999u32.to_be_bytes());
    payload.extend_from_slice(&512u32.to_be_bytes());
    executor.pass_with(payload);
    let mut scsi = transparent(executor);
    assert_eq!(
        scsi.read_capacity(0),
        Ok(BlockGeometry {
            block_size: 512,
            block_count: 2000,
        })
    );

    // A block size that is not a power of two in 512..=4096 fails closed.
    let mut executor = ScriptedExecutor::new();
    let mut payload = Vec::new();
    payload.extend_from_slice(&1999u32.to_be_bytes());
    payload.extend_from_slice(&520u32.to_be_bytes());
    executor.pass_with(payload);
    let mut scsi = transparent(executor);
    assert_eq!(scsi.read_capacity(0), Err(Errno::OutOfRange));
}

/// Geometry + state used by the `LunBlock` tests.
fn small_lun_state(write_protected: bool) -> LunState {
    LunState {
        geometry: BlockGeometry {
            block_size: 512,
            block_count: 1 << 30,
        },
        write_protected,
    }
}

#[test]
fn lun_block_refuses_writes_to_a_protected_medium_before_the_device() {
    let executor = ScriptedExecutor::new();
    let mut scsi = transparent(executor);
    let mut lun = LunBlock::new(&mut scsi, 0, small_lun_state(true));
    let buf = [0u8; 512];
    assert_eq!(
        lun.write_blocks(0, &buf),
        Err(DriverError::PermissionDenied)
    );
    // No command ever reached the device.
    assert!(scsi.transport().calls.is_empty());
}

#[test]
fn lun_block_validates_shape_and_range() {
    let executor = ScriptedExecutor::new();
    let mut scsi = transparent(executor);
    let state = LunState {
        geometry: BlockGeometry {
            block_size: 512,
            block_count: 8,
        },
        write_protected: false,
    };
    let mut lun = LunBlock::new(&mut scsi, 0, state);
    let mut torn = [0u8; 100];
    assert_eq!(
        lun.read_blocks(0, &mut torn),
        Err(DriverError::BufferTooSmall)
    );
    let mut past_end = [0u8; 512];
    assert_eq!(
        lun.read_blocks(8, &mut past_end),
        Err(DriverError::LengthOutOfRange)
    );
    assert!(scsi.transport().calls.is_empty());
}

#[test]
fn lun_block_chunks_a_large_read_into_bounded_commands() {
    let mut executor = ScriptedExecutor::new();
    // Two full-window chunks: each is one command.
    for _ in 0..2 {
        executor.pass_with(vec![0xEEu8; MSD_MAX_TRANSFER_LEN]);
    }
    let mut scsi = transparent(executor);
    let mut lun = LunBlock::new(&mut scsi, 0, small_lun_state(false));
    let mut buf = vec![0u8; 2 * MSD_MAX_TRANSFER_LEN];
    lun.read_blocks(0, &mut buf).expect("chunked read passes");
    assert!(buf.iter().all(|&b| b == 0xEE));

    // Two READ(10)s, the second starting where the first ended.
    let blocks_per_chunk = u32::try_from(MSD_MAX_TRANSFER_LEN / 512).expect("fits");
    let (_, cdb0, _, _) = &scsi.transport().calls[0];
    let (_, cdb1, _, _) = &scsi.transport().calls[1];
    assert_eq!(&cdb0[2..6], &0u32.to_be_bytes());
    assert_eq!(&cdb1[2..6], &blocks_per_chunk.to_be_bytes());
}

#[test]
fn a_sensitive_transfer_scrubs_the_shared_window() {
    let mut executor = ScriptedExecutor::new();
    executor.pass_with(vec![0u8; 512]);
    let mut scsi = transparent(executor);
    let mut lun = LunBlock::new(&mut scsi, 0, small_lun_state(false));
    let mut buf = [0u8; 512];
    lun.read_blocks_with_class(0, &mut buf, BufferClass::Sensitive)
        .expect("read passes");
    assert_eq!(scsi.transport().scrubs, 1);
}

#[test]
fn an_oversize_or_empty_command_block_is_refused() {
    let executor = ScriptedExecutor::new();
    let mut scsi = transparent(executor);
    let outcome = scsi.command(0, &[], DataPhase::None);
    assert_eq!(outcome.err(), Some(Errno::OutOfRange));
    let seventeen = [0u8; 17];
    let outcome = scsi.command(0, &seventeen, DataPhase::None);
    assert_eq!(outcome.err(), Some(Errno::OutOfRange));
    assert!(scsi.transport().calls.is_empty());
}
