//! Host tests for the CBI wire transport over a scripted device
//! (`plans/DEVICES.md` D5): ADSC framing, the stall-as-refusal answer,
//! data phases, both completion-block spellings, the Command Block Reset
//! trail, and a UFI floppy exchange through the shared SCSI layer.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::block::BlockGeometry;

use crate::scsi::{CommandSet, ScsiDevice};
use crate::testutil::{InStep, ScriptedDevice};

/// A UFI floppy: the shared SCSI layer over a CBI transport around
/// `device` — the shape the `Run` binary builds for interface 1.
fn floppy(device: ScriptedDevice) -> ScsiDevice<Cbi<ScriptedDevice>> {
    ScsiDevice::new(Cbi::new(device, 1, CbiStatus::UfiSense), CommandSet::Ufi)
}

/// Borrow the scripted device back out of the layered stack.
fn device(scsi: &ScsiDevice<Cbi<ScriptedDevice>>) -> &ScriptedDevice {
    scsi.transport().transport()
}

/// The UFI "all good" completion block.
const PASSED: [u8; 2] = [0, 0];

#[test]
fn a_command_rides_adsc_as_a_twelve_byte_block() {
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(PASSED.to_vec());
    let mut scsi = floppy(dev);
    assert_eq!(scsi.test_unit_ready(0), Ok(true));

    let (setup, block) = &device(&scsi).control_outs[0];
    // ADSC: class-specific OUT to interface 1 carrying 12 bytes.
    assert_eq!(setup, &[0x21, 0x00, 0, 0, 1, 0, 12, 0]);
    assert_eq!(block.len(), 12);
    assert!(block.iter().all(|&b| b == 0)); // TEST UNIT READY, padded.
}

#[test]
fn a_read_moves_the_data_phase_and_checks_the_completion() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0xA5u8; 512]));
    dev.interrupts.push_back(PASSED.to_vec());
    let mut scsi = floppy(dev);

    let mut buf = [0u8; 512];
    scsi.read(0, 9, 512, &mut buf).expect("read passes");
    assert_eq!(buf, [0xA5u8; 512]);
    let (_, block) = &device(&scsi).control_outs[0];
    // READ(10) padded to 12 bytes: opcode, LBA 9, one block.
    assert_eq!(block.as_slice(), &[0x28, 0, 0, 0, 0, 9, 0, 0, 1, 0, 0, 0]);
}

#[test]
fn a_write_moves_the_out_phase_over_bulk() {
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(PASSED.to_vec());
    let mut scsi = floppy(dev);
    let payload = [0x5Au8; 512];
    scsi.write(0, 4, 512, &payload).expect("write passes");
    assert_eq!(device(&scsi).out_frames[0], payload.to_vec());
}

#[test]
fn a_nonzero_completion_is_a_failed_command_with_sense() {
    // The UFI completion block IS the sense: a WRITE PROTECTED ASC (0x27)
    // is read in-band, so no separate REQUEST SENSE round trip is issued.
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.interrupts.push_back(vec![0x27, 0x00]); // ASC: WRITE PROTECTED
    let mut scsi = floppy(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::PermissionDenied));
    // Exactly one command reached the wire (the READ); the verdict came
    // from its completion block, not a follow-up REQUEST SENSE.
    assert_eq!(device(&scsi).control_outs.len(), 1);
}

#[test]
fn an_adsc_stall_is_the_command_not_accepted_answer() {
    let mut dev = ScriptedDevice::new();
    dev.stall_next_control_out = true;
    // The REQUEST SENSE that follows the refusal: ILLEGAL REQUEST.
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x05;
    sense[12] = 0x20;
    dev.in_steps.push_back(InStep::Data(sense));
    dev.interrupts.push_back(PASSED.to_vec());
    let mut scsi = floppy(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::OutOfRange));
    // No bulk data moved and no completion interrupt was read for the
    // refused command.
    assert!(device(&scsi).out_frames.is_empty());
}

#[test]
fn a_stalled_data_phase_still_reads_the_completion() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Stall);
    dev.interrupts.push_back(vec![0x3A, 0x00]); // MEDIUM NOT PRESENT
    let mut scsi = floppy(dev);
    let mut buf = [0u8; 512];
    // The stalled data phase still reads the completion, whose ASC (0x3A,
    // medium not present) maps to NOT READY in-band — no REQUEST SENSE.
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::WouldBlock));
    assert_eq!(device(&scsi).control_outs.len(), 1);
}

#[test]
fn a_malformed_completion_block_runs_command_block_reset() {
    let mut dev = ScriptedDevice::new();
    dev.in_steps.push_back(InStep::Data(vec![0u8; 512]));
    dev.interrupts.push_back(vec![0x00]); // one byte: wrong shape
    let mut scsi = floppy(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::LengthOutOfRange));
    // The reset is the *second* ADSC: SEND DIAGNOSTIC with the reset pads.
    let (_, reset) = &device(&scsi).control_outs[1];
    assert_eq!(reset[0], 0x1D);
    assert_eq!(reset[1], 0x04);
    assert!(reset[2..].iter().all(|&b| b == 0xFF));
}

#[test]
fn the_typed_status_spelling_is_honoured() {
    // A non-UFI command set over CBI reports a typed status value.
    fn typed(dev: ScriptedDevice) -> Cbi<ScriptedDevice> {
        Cbi::new(dev, 0, CbiStatus::CommandStatus)
    }
    use crate::scsi::{DataPhase, ScsiTransport};

    // Passed.
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(vec![0x00, 0x00]);
    let mut cbi = typed(dev);
    let outcome = cbi.execute(0, &[0u8; 12], DataPhase::None).expect("runs");
    assert!(outcome.passed);

    // Failed.
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(vec![0x00, 0x01]);
    let mut cbi = typed(dev);
    let outcome = cbi.execute(0, &[0u8; 12], DataPhase::None).expect("runs");
    assert!(!outcome.passed);

    // Phase error: reset and surface.
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(vec![0x00, 0x02]);
    let mut cbi = typed(dev);
    assert_eq!(
        cbi.execute(0, &[0u8; 12], DataPhase::None),
        Err(Errno::NotImplemented)
    );
    assert_eq!(cbi.transport().control_outs.len(), 2); // command + reset

    // A block whose first byte is not a Command Completion Interrupt.
    let mut dev = ScriptedDevice::new();
    dev.interrupts.push_back(vec![0x11, 0x00]);
    let mut cbi = typed(dev);
    assert_eq!(
        cbi.execute(0, &[0u8; 12], DataPhase::None),
        Err(Errno::BadMagic)
    );
}

#[test]
fn cbi_serves_exactly_one_lun_and_refuses_others() {
    let dev = ScriptedDevice::new();
    let mut scsi = floppy(dev);
    assert_eq!(scsi.lun_count(), Ok(1));
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(1, 0, 512, &mut buf), Err(Errno::OutOfRange));
    assert!(device(&scsi).control_outs.is_empty());
}

#[test]
fn an_oversize_command_block_is_refused_before_the_wire() {
    use crate::scsi::{DataPhase, ScsiTransport};
    let dev = ScriptedDevice::new();
    let mut cbi = Cbi::new(dev, 0, CbiStatus::UfiSense);
    let thirteen = [0u8; 13];
    assert_eq!(
        cbi.execute(0, &thirteen, DataPhase::None),
        Err(Errno::OutOfRange)
    );
    assert!(cbi.transport().control_outs.is_empty());
}

#[test]
fn a_not_ready_floppy_drains_via_in_band_sense_without_request_sense() {
    // Regression: a real UFI floppy (a Mitsumi SmartDisk FDD) returns a
    // start-of-day not-ready / UNIT ATTENTION on TEST UNIT READY, and does
    // not reliably answer a separate REQUEST SENSE. The bounded ready drain
    // must read the sense in-band from the completion block and keep
    // draining until the unit is ready, never issuing a REQUEST SENSE whose
    // failure would abort bring-up (the observed `errno 0xc` metal defect).
    let mut dev = ScriptedDevice::new();
    // TEST UNIT READY carries no data phase: script completion blocks only.
    dev.interrupts.push_back(vec![0x3A, 0x00]); // not ready (medium not present)
    dev.interrupts.push_back(vec![0x28, 0x00]); // unit attention (media change)
    dev.interrupts.push_back(PASSED.to_vec()); // ready
    let mut scsi = floppy(dev);
    assert_eq!(scsi.ready_after_drain(0, 8), Ok(true));
    // Three TEST UNIT READY commands reached the wire and nothing else: no
    // REQUEST SENSE (which would need a bulk data phase the script never
    // provides and, on the real drive, would fail the command).
    assert_eq!(device(&scsi).control_outs.len(), 3);
    assert!(device(&scsi).in_steps.is_empty());
}

#[test]
fn a_floppy_geometry_reads_through_the_ufi_set() {
    // READ CAPACITY on a 1.44 MB floppy: 2880 blocks of 512 bytes.
    let mut dev = ScriptedDevice::new();
    let mut payload = Vec::new();
    payload.extend_from_slice(&2879u32.to_be_bytes());
    payload.extend_from_slice(&512u32.to_be_bytes());
    dev.in_steps.push_back(InStep::Data(payload));
    dev.interrupts.push_back(PASSED.to_vec());
    let mut scsi = floppy(dev);
    assert_eq!(
        scsi.read_capacity(0),
        Ok(BlockGeometry {
            block_size: 512,
            block_count: 2880,
        })
    );
    // The CDB rode as a 12-byte padded block.
    let (_, block) = &device(&scsi).control_outs[0];
    assert_eq!(block[0], 0x25);
    assert_eq!(block.len(), 12);
}
