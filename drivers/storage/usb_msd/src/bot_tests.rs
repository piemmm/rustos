//! Host tests for the BOT + SCSI engine over a scripted device
//! (`plans/DEVICES.md` D2): CBW/CSW framing, tag mismatch, stall
//! recovery, short reads, write-protect, and multi-LUN bring-up.

use super::*;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// One scripted answer to a bulk-IN transfer.
enum InStep {
    /// Deliver these bytes (short if fewer than requested).
    Data(Vec<u8>),
    /// STALL the transfer (the transport reports the endpoint recovered).
    Stall,
}

/// A scripted BOT device behind the [`MsdTransport`] seam.
struct ScriptedDevice {
    /// Queued bulk-IN answers, consumed one per transfer.
    in_steps: VecDeque<InStep>,
    /// Every bulk-OUT payload the device accepted (CBWs and data).
    out_frames: Vec<Vec<u8>>,
    /// Whether the next bulk-OUT STALLs (consumed once).
    stall_next_out: bool,
    /// Recorded no-data control SETUPs (the BOT reset trail).
    resets: Vec<[u8; 8]>,
    /// Scripted `GET MAX LUN` answer; `None` STALLs the request.
    max_lun: Option<u8>,
    /// Window-scrub call count.
    scrubs: usize,
}

impl ScriptedDevice {
    fn new() -> Self {
        Self {
            in_steps: VecDeque::new(),
            out_frames: Vec::new(),
            stall_next_out: false,
            resets: Vec::new(),
            max_lun: Some(0),
            scrubs: 0,
        }
    }

    /// Queue a passing CSW for `tag` with `residue`.
    fn queue_csw(&mut self, tag: u32, status: u8, residue: u32) {
        let mut csw = vec![0u8; CSW_LEN];
        csw[0..4].copy_from_slice(&0x5342_5355u32.to_le_bytes());
        csw[4..8].copy_from_slice(&tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = status;
        self.in_steps.push_back(InStep::Data(csw));
    }
}

impl MsdTransport for ScriptedDevice {
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, Errno> {
        // The only control-IN the engine issues is GET MAX LUN.
        assert_eq!(setup[0], 0xA1);
        assert_eq!(setup[1], 0xFE);
        match self.max_lun {
            Some(value) => {
                data[0] = value;
                Ok(1)
            }
            None => Err(Errno::EndpointStalled),
        }
    }

    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), Errno> {
        self.resets.push(setup);
        Ok(())
    }

    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize, Errno> {
        match self.in_steps.pop_front() {
            Some(InStep::Data(bytes)) => {
                let n = bytes.len().min(data.len());
                data[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            Some(InStep::Stall) => Err(Errno::EndpointStalled),
            None => Err(Errno::NotFound),
        }
    }

    fn bulk_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        if self.stall_next_out {
            self.stall_next_out = false;
            return Err(Errno::EndpointStalled);
        }
        self.out_frames.push(data.to_vec());
        Ok(data.len())
    }

    fn scrub(&mut self) {
        self.scrubs += 1;
    }
}

/// The tag the engine's first command carries.
const FIRST_TAG: u32 = 1;

#[test]
fn lun_count_reads_get_max_lun_and_stall_means_one() {
    let mut device = ScriptedDevice::new();
    device.max_lun = Some(3);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.lun_count(), Ok(4));

    let mut device = ScriptedDevice::new();
    device.max_lun = None; // STALL: no multi-LUN support.
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.lun_count(), Ok(1));

    let mut device = ScriptedDevice::new();
    device.max_lun = Some(16); // Protocol violation.
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.lun_count(), Err(Errno::OutOfRange));
}

/// Decode the interesting CBW fields of a captured bulk-OUT frame.
fn parse_cbw(frame: &[u8]) -> (u32, u32, u8, u8, &[u8]) {
    assert_eq!(frame.len(), CBW_LEN);
    assert_eq!(
        u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]),
        0x4342_5355
    );
    let tag = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
    let dtl = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);
    let cb_len = usize::from(frame[14]);
    (tag, dtl, frame[12], frame[13], &frame[15..15 + cb_len])
}

#[test]
fn a_read_frames_the_cbw_and_returns_the_payload() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0xA5u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);

    let mut buf = [0u8; 512];
    msd.read(2, 9, 512, &mut buf).expect("read passes");
    assert_eq!(buf, [0xA5u8; 512]);

    let (tag, dtl, flags, lun, cb) = parse_cbw(&msd.transport.out_frames[0]);
    assert_eq!(tag, FIRST_TAG);
    assert_eq!(dtl, 512);
    assert_eq!(flags, 0x80); // device-to-host
    assert_eq!(lun, 2);
    // READ(10): opcode, big-endian LBA, big-endian block count.
    assert_eq!(cb, &[0x28, 0, 0, 0, 0, 9, 0, 0, 1, 0]);
}

#[test]
fn a_write_moves_the_data_phase_to_the_device() {
    let mut device = ScriptedDevice::new();
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);

    let payload = [0x5Au8; 1024];
    msd.write(0, 4, 512, &payload).expect("write passes");

    let (_, dtl, flags, _, cb) = parse_cbw(&msd.transport.out_frames[0]);
    assert_eq!(dtl, 1024);
    assert_eq!(flags, 0); // host-to-device
    assert_eq!(cb, &[0x2A, 0, 0, 0, 0, 4, 0, 0, 2, 0]);
    // The data phase followed the CBW, byte for byte.
    assert_eq!(msd.transport.out_frames[1], payload.to_vec());
}

#[test]
fn a_far_range_uses_the_sixteen_byte_command() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);

    let lba = u64::from(u32::MAX) + 5;
    let mut buf = [0u8; 512];
    msd.read(0, lba, 512, &mut buf).expect("read passes");
    let (_, _, _, _, cb) = parse_cbw(&msd.transport.out_frames[0]);
    assert_eq!(cb[0], 0x88); // READ(16)
    assert_eq!(cb[2..10], lba.to_be_bytes());
    assert_eq!(cb[10..14], 1u32.to_be_bytes());
}

#[test]
fn a_mismatched_csw_tag_runs_reset_recovery_and_fails() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG + 7, 0, 0); // stale tag
    let mut msd = Msd::new(device, 3);

    let mut buf = [0u8; 512];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    // Exactly one Bulk-Only Mass Storage Reset, addressed to interface 3.
    assert_eq!(msd.transport.resets, vec![[0x21, 0xFF, 0, 0, 3, 0, 0, 0]]);
}

#[test]
fn a_corrupt_csw_signature_or_residue_fails_closed() {
    // Bad signature.
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    let mut csw = vec![0u8; CSW_LEN];
    csw[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    csw[4..8].copy_from_slice(&FIRST_TAG.to_le_bytes());
    device.in_steps.push_back(InStep::Data(csw));
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    assert_eq!(msd.transport.resets.len(), 1);

    // Residue larger than the transfer it describes.
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 513);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    assert_eq!(msd.transport.resets.len(), 1);
}

#[test]
fn a_phase_error_runs_reset_recovery() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 2, 0); // phase error
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
    assert_eq!(msd.transport.resets.len(), 1);
}

#[test]
fn a_stalled_csw_read_is_retried_once() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0x11u8; 512]));
    device.in_steps.push_back(InStep::Stall); // first CSW attempt STALLs
    device.queue_csw(FIRST_TAG, 0, 0); // the retry succeeds
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    msd.read(0, 0, 512, &mut buf)
        .expect("read passes after retry");
    assert!(msd.transport.resets.is_empty());
}

#[test]
fn a_stalled_data_phase_falls_through_to_the_csw() {
    // The device STALLs the data phase and reports the whole transfer as
    // residue: the read fails honestly, with no reset needed (the
    // transport already recovered the endpoint).
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Stall);
    device.queue_csw(FIRST_TAG, 0, 512);
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
    assert!(msd.transport.resets.is_empty());
}

#[test]
fn a_short_read_the_device_calls_success_is_refused() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 512); // half of 1024 missing
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 1024];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
}

#[test]
fn a_failed_read_maps_the_sense_key() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 1, 512); // CHECK CONDITION
                                         // The REQUEST SENSE that follows: fixed-format DATA PROTECT.
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x07; // DATA PROTECT
    sense[12] = 0x27;
    device.in_steps.push_back(InStep::Data(sense));
    device.queue_csw(FIRST_TAG + 1, 0, 0);
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    assert_eq!(msd.read(0, 0, 512, &mut buf), Err(Errno::PermissionDenied));
}

#[test]
fn inquiry_accepts_a_short_but_sufficient_answer() {
    let mut device = ScriptedDevice::new();
    device
        .in_steps
        .push_back(InStep::Data(vec![0x00, 0x80, 0x05, 0x02, 0x1F]));
    device.queue_csw(FIRST_TAG, 0, 31);
    let mut msd = Msd::new(device, 0);
    assert_eq!(
        msd.inquiry(0),
        Ok(Inquiry {
            device_type: DEVICE_TYPE_DIRECT_ACCESS,
            removable: true,
        })
    );
}

#[test]
fn read_capacity_ten_form_and_validation() {
    let mut device = ScriptedDevice::new();
    // Max LBA 1999, block size 512 → 2000 blocks.
    let mut payload = Vec::new();
    payload.extend_from_slice(&1999u32.to_be_bytes());
    payload.extend_from_slice(&512u32.to_be_bytes());
    device.in_steps.push_back(InStep::Data(payload));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);
    assert_eq!(
        msd.read_capacity(0),
        Ok(BlockGeometry {
            block_size: 512,
            block_count: 2000,
        })
    );

    // A block size that is not a power of two in 512..=4096 fails closed.
    let mut device = ScriptedDevice::new();
    let mut payload = Vec::new();
    payload.extend_from_slice(&1999u32.to_be_bytes());
    payload.extend_from_slice(&520u32.to_be_bytes());
    device.in_steps.push_back(InStep::Data(payload));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.read_capacity(0), Err(Errno::OutOfRange));
}

#[test]
fn read_capacity_falls_back_to_the_sixteen_form_for_huge_units() {
    let mut device = ScriptedDevice::new();
    // READ CAPACITY(10) saturates...
    let mut payload = Vec::new();
    payload.extend_from_slice(&u32::MAX.to_be_bytes());
    payload.extend_from_slice(&512u32.to_be_bytes());
    device.in_steps.push_back(InStep::Data(payload));
    device.queue_csw(FIRST_TAG, 0, 0);
    // ...so READ CAPACITY(16) answers: a 100 TB-class unit.
    let max_lba: u64 = 0x30_0000_0000;
    let mut payload = vec![0u8; 32];
    payload[0..8].copy_from_slice(&max_lba.to_be_bytes());
    payload[8..12].copy_from_slice(&4096u32.to_be_bytes());
    device.in_steps.push_back(InStep::Data(payload));
    device.queue_csw(FIRST_TAG + 1, 0, 0);
    let mut msd = Msd::new(device, 0);
    assert_eq!(
        msd.read_capacity(0),
        Ok(BlockGeometry {
            block_size: 4096,
            block_count: max_lba + 1,
        })
    );
    let (_, _, _, _, cb) = parse_cbw(&msd.transport.out_frames[1]);
    assert_eq!(cb[0], 0x9E);
    assert_eq!(cb[1], 0x10);
}

#[test]
fn write_protect_reads_the_mode_sense_wp_bit() {
    let mut device = ScriptedDevice::new();
    device
        .in_steps
        .push_back(InStep::Data(vec![0x03, 0x00, 0x80, 0x00]));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.write_protected(0), Ok(true));

    // A device that fails MODE SENSE is reported write-enabled.
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(Vec::new()));
    device.queue_csw(FIRST_TAG, 1, 4);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.write_protected(0), Ok(false));
}

#[test]
fn synchronize_cache_treats_illegal_request_as_no_cache() {
    let mut device = ScriptedDevice::new();
    device.queue_csw(FIRST_TAG, 1, 0); // flush "fails"...
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x05; // ...because ILLEGAL REQUEST: no cache to flush.
    device.in_steps.push_back(InStep::Data(sense));
    device.queue_csw(FIRST_TAG + 1, 0, 0);
    let mut msd = Msd::new(device, 0);
    assert_eq!(msd.synchronize_cache(0), Ok(()));
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
    let device = ScriptedDevice::new();
    let mut msd = Msd::new(device, 0);
    let mut lun = LunBlock::new(&mut msd, 0, small_lun_state(true));
    let buf = [0u8; 512];
    assert_eq!(
        lun.write_blocks(0, &buf),
        Err(DriverError::PermissionDenied)
    );
    // No CBW ever reached the device.
    assert!(msd.transport.out_frames.is_empty());
}

#[test]
fn lun_block_validates_shape_and_range() {
    let device = ScriptedDevice::new();
    let mut msd = Msd::new(device, 0);
    let state = LunState {
        geometry: BlockGeometry {
            block_size: 512,
            block_count: 8,
        },
        write_protected: false,
    };
    let mut lun = LunBlock::new(&mut msd, 0, state);
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
    assert!(msd.transport.out_frames.is_empty());
}

#[test]
fn lun_block_chunks_a_large_read_into_bounded_commands() {
    let mut device = ScriptedDevice::new();
    // Two full-window chunks: each is one BOT command.
    for tag in [FIRST_TAG, FIRST_TAG + 1] {
        device
            .in_steps
            .push_back(InStep::Data(vec![0xEEu8; MSD_MAX_TRANSFER_LEN]));
        device.queue_csw(tag, 0, 0);
    }
    let mut msd = Msd::new(device, 0);
    let mut lun = LunBlock::new(&mut msd, 0, small_lun_state(false));
    let mut buf = vec![0u8; 2 * MSD_MAX_TRANSFER_LEN];
    lun.read_blocks(0, &mut buf).expect("chunked read passes");
    assert!(buf.iter().all(|&b| b == 0xEE));

    // Two CBWs, the second starting where the first ended.
    let (_, _, _, _, cb0) = parse_cbw(&msd.transport.out_frames[0]);
    let (_, _, _, _, cb1) = parse_cbw(&msd.transport.out_frames[1]);
    let blocks_per_chunk = u32::try_from(MSD_MAX_TRANSFER_LEN / 512).expect("fits");
    assert_eq!(&cb0[2..6], &0u32.to_be_bytes());
    assert_eq!(&cb1[2..6], &blocks_per_chunk.to_be_bytes());
}

#[test]
fn a_sensitive_transfer_scrubs_the_shared_window() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);
    let mut lun = LunBlock::new(&mut msd, 0, small_lun_state(false));
    let mut buf = [0u8; 512];
    lun.read_blocks_with_class(0, &mut buf, BufferClass::Sensitive)
        .expect("read passes");
    assert_eq!(msd.transport.scrubs, 1);
}

#[test]
fn multi_lun_commands_carry_the_lun_byte() {
    let mut device = ScriptedDevice::new();
    device.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    device.queue_csw(FIRST_TAG, 0, 0);
    let mut msd = Msd::new(device, 0);
    let mut buf = [0u8; 512];
    msd.read(5, 0, 512, &mut buf).expect("read passes");
    let (_, _, _, lun, _) = parse_cbw(&msd.transport.out_frames[0]);
    assert_eq!(lun, 5);

    // A LUN outside the protocol bound never reaches the wire.
    assert_eq!(msd.read(16, 0, 512, &mut buf), Err(Errno::OutOfRange));
    assert_eq!(msd.transport.out_frames.len(), 1);
}
