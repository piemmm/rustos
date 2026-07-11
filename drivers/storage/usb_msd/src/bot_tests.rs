//! Host tests for the BOT wire transport over a scripted device
//! (`plans/DEVICES.md` D2/D5): CBW/CSW framing, tag mismatch, stall
//! recovery, short reads, and the LUN count — with the shared SCSI layer
//! driving it exactly as the `Run` binary does.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::block::BlockGeometry;

use crate::scsi::{CommandSet, ScsiDevice};
use crate::testutil::{InStep, ScriptedDevice};

/// The tag the transport's first command carries.
const FIRST_TAG: u32 = 1;

/// The shared SCSI layer over a BOT transport around `device`, speaking
/// the transparent set — the shape the `Run` binary builds for a stick.
fn scsi(device: ScriptedDevice) -> ScsiDevice<Bot<ScriptedDevice>> {
    ScsiDevice::new(Bot::new(device, 0), CommandSet::Transparent)
}

/// Borrow the scripted device back out of the layered stack.
fn device(scsi: &ScsiDevice<Bot<ScriptedDevice>>) -> &ScriptedDevice {
    scsi.transport().transport()
}

#[test]
fn lun_count_reads_get_max_lun_and_stall_means_one() {
    let mut dev = ScriptedDevice::new();
    dev.max_lun = Some(3);
    let mut scsi = scsi(dev);
    assert_eq!(scsi.lun_count(), Ok(4));

    let mut dev = ScriptedDevice::new();
    dev.max_lun = None; // STALL: no multi-LUN support.
    let mut scsi = self::scsi(dev);
    assert_eq!(scsi.lun_count(), Ok(1));

    let mut dev = ScriptedDevice::new();
    dev.max_lun = Some(16); // Protocol violation.
    let mut scsi = self::scsi(dev);
    assert_eq!(scsi.lun_count(), Err(Errno::OutOfRange));
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
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0xA5u8; 512]));
    dev.queue_csw(FIRST_TAG, 0, 0);
    let mut scsi = scsi(dev);

    let mut buf = [0u8; 512];
    scsi.read(2, 9, 512, &mut buf).expect("read passes");
    assert_eq!(buf, [0xA5u8; 512]);

    let (tag, dtl, flags, lun, cb) = parse_cbw(&device(&scsi).out_frames[0]);
    assert_eq!(tag, FIRST_TAG);
    assert_eq!(dtl, 512);
    assert_eq!(flags, 0x80); // device-to-host
    assert_eq!(lun, 2);
    // READ(10): opcode, big-endian LBA, big-endian block count.
    assert_eq!(cb, &[0x28, 0, 0, 0, 0, 9, 0, 0, 1, 0]);
}

#[test]
fn a_write_moves_the_data_phase_to_the_device() {
    let mut dev = ScriptedDevice::new();
    dev.queue_csw(FIRST_TAG, 0, 0);
    let mut scsi = scsi(dev);

    let payload = [0x5Au8; 1024];
    scsi.write(0, 4, 512, &payload).expect("write passes");

    let (_, dtl, flags, _, cb) = parse_cbw(&device(&scsi).out_frames[0]);
    assert_eq!(dtl, 1024);
    assert_eq!(flags, 0); // host-to-device
    assert_eq!(cb, &[0x2A, 0, 0, 0, 0, 4, 0, 0, 2, 0]);
    // The data phase followed the CBW, byte for byte.
    assert_eq!(device(&scsi).out_frames[1], payload.to_vec());
}

#[test]
fn a_ufi_device_pads_the_command_block_to_twelve_bytes() {
    // A UFI floppy over BOT (sub-class 0x04, protocol 0x50): the same
    // framing, but every CDB rides as a 12-byte zero-padded block.
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0x33u8; 512]));
    dev.queue_csw(FIRST_TAG, 0, 0);
    let mut scsi = ScsiDevice::new(Bot::new(dev, 0), CommandSet::Ufi);

    let mut buf = [0u8; 512];
    scsi.read(0, 1, 512, &mut buf).expect("read passes");
    let (_, _, _, _, cb) = parse_cbw(&scsi.transport().transport().out_frames[0]);
    assert_eq!(cb.len(), 12);
    assert_eq!(&cb[..10], &[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
    assert_eq!(&cb[10..], &[0, 0]);
}

#[test]
fn a_mismatched_csw_tag_runs_reset_recovery_and_fails() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG + 7, 0, 0); // stale tag
    let mut scsi = ScsiDevice::new(Bot::new(dev, 3), CommandSet::Transparent);

    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    // Exactly one Bulk-Only Mass Storage Reset, addressed to interface 3.
    assert_eq!(
        scsi.transport().transport().resets,
        vec![[0x21, 0xFF, 0, 0, 3, 0, 0, 0]]
    );
}

#[test]
fn a_corrupt_csw_signature_or_residue_fails_closed() {
    // Bad signature.
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    let mut csw = vec![0u8; CSW_LEN];
    csw[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    csw[4..8].copy_from_slice(&FIRST_TAG.to_le_bytes());
    dev.in_steps.push_back(InStep::Data(csw));
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    assert_eq!(device(&scsi).resets.len(), 1);

    // Residue larger than the transfer it describes.
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG, 0, 513);
    let mut scsi = self::scsi(dev);
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
    assert_eq!(device(&scsi).resets.len(), 1);
}

#[test]
fn a_phase_error_runs_reset_recovery() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG, 2, 0); // phase error
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
    assert_eq!(device(&scsi).resets.len(), 1);
}

#[test]
fn a_stalled_csw_read_is_retried_once() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0x11u8; 512]));
    dev.in_steps.push_back(InStep::Stall); // first CSW attempt STALLs
    dev.queue_csw(FIRST_TAG, 0, 0); // the retry succeeds
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    scsi.read(0, 0, 512, &mut buf)
        .expect("read passes after retry");
    assert!(device(&scsi).resets.is_empty());
}

#[test]
fn a_stalled_data_phase_falls_through_to_the_csw() {
    // The device STALLs the data phase and reports the whole transfer as
    // residue: the read fails honestly, with no reset needed (the
    // transport already recovered the endpoint).
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Stall);
    dev.queue_csw(FIRST_TAG, 0, 512);
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
    assert!(device(&scsi).resets.is_empty());
}

#[test]
fn a_short_read_the_device_calls_success_is_refused() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG, 0, 512); // half of 1024 missing
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 1024];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::NotImplemented));
}

#[test]
fn a_failed_read_maps_the_sense_key() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG, 1, 512); // CHECK CONDITION
                                      // The REQUEST SENSE that follows: fixed-format DATA PROTECT.
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x07; // DATA PROTECT
    sense[12] = 0x27;
    dev.in_steps.push_back(InStep::Data(sense));
    dev.queue_csw(FIRST_TAG + 1, 0, 0);
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::PermissionDenied));
}

#[test]
fn read_capacity_falls_back_to_the_sixteen_form_for_huge_units() {
    let mut dev = ScriptedDevice::new();
    // READ CAPACITY(10) saturates...
    let mut payload = Vec::new();
    payload.extend_from_slice(&u32::MAX.to_be_bytes());
    payload.extend_from_slice(&512u32.to_be_bytes());
    dev.in_steps.push_back(InStep::Data(payload));
    dev.queue_csw(FIRST_TAG, 0, 0);
    // ...so READ CAPACITY(16) answers: a 100 TB-class unit.
    let max_lba: u64 = 0x30_0000_0000;
    let mut payload = vec![0u8; 32];
    payload[0..8].copy_from_slice(&max_lba.to_be_bytes());
    payload[8..12].copy_from_slice(&4096u32.to_be_bytes());
    dev.in_steps.push_back(InStep::Data(payload));
    dev.queue_csw(FIRST_TAG + 1, 0, 0);
    let mut scsi = scsi(dev);
    assert_eq!(
        scsi.read_capacity(0),
        Ok(BlockGeometry {
            block_size: 4096,
            block_count: max_lba + 1,
        })
    );
    let (_, _, _, _, cb) = parse_cbw(&device(&scsi).out_frames[1]);
    assert_eq!(cb[0], 0x9E);
    assert_eq!(cb[1], 0x10);
}

#[test]
fn multi_lun_commands_carry_the_lun_byte() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.queue_csw(FIRST_TAG, 0, 0);
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    scsi.read(5, 0, 512, &mut buf).expect("read passes");
    let (_, _, _, lun, _) = parse_cbw(&device(&scsi).out_frames[0]);
    assert_eq!(lun, 5);

    // A LUN outside the protocol bound never reaches the wire.
    assert_eq!(scsi.read(16, 0, 512, &mut buf), Err(Errno::OutOfRange));
    assert_eq!(device(&scsi).out_frames.len(), 1);
}
