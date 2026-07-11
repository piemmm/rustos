//! Host tests for the UAS IU engine over scripted pipes
//! (`plans/DEVICES.md` D5): the command/ready/data/sense sequencing of
//! USB 2.0 non-stream operation, tag checking, autosense in both sense
//! formats, hostile-IU refusals, and `REPORT LUNS` unit discovery.

use super::*;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use crate::scsi::{CommandSet, ScsiDevice};

/// The four scripted pipes of one UAS device.
struct ScriptedPipes {
    /// IUs the host sent on the command pipe.
    commands: Vec<Vec<u8>>,
    /// Queued status-pipe IUs. A `TAG` placeholder in bytes 2..4 is
    /// rewritten to the tag of the most recent Command IU, so scripts
    /// need not predict tag values.
    status: VecDeque<Vec<u8>>,
    /// Whether status IUs echo the live tag (`false` scripts a stale tag).
    echo_tag: bool,
    /// Queued data-in payloads, one per Read Ready.
    data_in: VecDeque<Vec<u8>>,
    /// Data the host wrote to the data-out pipe.
    data_out: Vec<Vec<u8>>,
    scrubs: usize,
}

impl ScriptedPipes {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
            status: VecDeque::new(),
            echo_tag: true,
            data_in: VecDeque::new(),
            data_out: Vec::new(),
            scrubs: 0,
        }
    }

    /// Queue a Sense IU with `status` and raw `sense` data.
    fn queue_sense(&mut self, status: u8, sense: &[u8]) {
        let mut iu = vec![0u8; 16 + sense.len()];
        iu[0] = 0x03;
        iu[6] = status;
        let len = u16::try_from(sense.len()).expect("fits");
        iu[14..16].copy_from_slice(&len.to_be_bytes());
        iu[16..].copy_from_slice(sense);
        self.status.push_back(iu);
    }

    /// Queue a Read Ready (`0x25`) or Write Ready (`0x24`) IU.
    fn queue_ready(&mut self, id: u8) {
        self.status.push_back(vec![id, 0, 0, 0]);
    }

    /// The tag of the most recent Command IU.
    fn live_tag(&self) -> [u8; 2] {
        let iu = self.commands.last().expect("a command was sent");
        [iu[2], iu[3]]
    }
}

impl UasPipes for ScriptedPipes {
    fn command_out(&mut self, iu: &[u8]) -> Result<(), Errno> {
        self.commands.push(iu.to_vec());
        Ok(())
    }

    fn status_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut iu = self.status.pop_front().ok_or(Errno::NotFound)?;
        if self.echo_tag && iu.len() >= 4 {
            let tag = self.live_tag();
            iu[2..4].copy_from_slice(&tag);
        }
        let n = iu.len().min(buf.len());
        buf[..n].copy_from_slice(&iu[..n]);
        Ok(n)
    }

    fn data_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let bytes = self.data_in.pop_front().ok_or(Errno::NotFound)?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn data_out(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        self.data_out.push(buf.to_vec());
        Ok(buf.len())
    }

    fn scrub(&mut self) {
        self.scrubs += 1;
    }
}

/// The shared SCSI layer over a UAS transport — the shape the `Run`
/// binary builds (UAS is always the transparent set).
fn scsi(pipes: ScriptedPipes) -> ScsiDevice<Uas<ScriptedPipes>> {
    ScsiDevice::new(Uas::new(pipes), CommandSet::Transparent)
}

/// Borrow the scripted pipes back out of the layered stack.
fn pipes(scsi: &ScsiDevice<Uas<ScriptedPipes>>) -> &ScriptedPipes {
    scsi.transport().pipes()
}

/// GOOD status with no sense data.
fn good() -> Vec<u8> {
    let mut iu = vec![0u8; 16];
    iu[0] = 0x03;
    iu
}

#[test]
fn a_read_sequences_command_ready_data_sense() {
    let mut dev = ScriptedPipes::new();
    dev.queue_ready(0x25); // Read Ready
    dev.data_in.push_back(vec![0xA5u8; 512]);
    dev.status.push_back(good());
    let mut scsi = scsi(dev);

    let mut buf = [0u8; 512];
    scsi.read(0, 9, 512, &mut buf).expect("read passes");
    assert_eq!(buf, [0xA5u8; 512]);

    // The Command IU: id, tag, SIMPLE attribute, LUN 0, READ(10).
    let iu = &pipes(&scsi).commands[0];
    assert_eq!(iu.len(), COMMAND_IU_LEN);
    assert_eq!(iu[0], 0x01);
    assert_ne!(u16::from_be_bytes([iu[2], iu[3]]), 0); // tag never 0
    assert_eq!(iu[16], 0x28); // READ(10)
}

#[test]
fn a_write_waits_for_write_ready_before_moving_data() {
    let mut dev = ScriptedPipes::new();
    dev.queue_ready(0x24); // Write Ready
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    let payload = [0x5Au8; 512];
    scsi.write(0, 4, 512, &payload).expect("write passes");
    assert_eq!(pipes(&scsi).data_out[0], payload.to_vec());
}

#[test]
fn a_no_data_command_finishes_on_the_sense_iu_alone() {
    let mut dev = ScriptedPipes::new();
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    assert_eq!(scsi.test_unit_ready(0), Ok(true));
}

#[test]
fn check_condition_delivers_autosense_in_fixed_format() {
    let mut dev = ScriptedPipes::new();
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x07; // DATA PROTECT
    sense[12] = 0x27;
    dev.queue_sense(0x02, &sense); // CHECK CONDITION
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::PermissionDenied));
    // No REQUEST SENSE round trip: one command only.
    assert_eq!(pipes(&scsi).commands.len(), 1);
}

#[test]
fn check_condition_delivers_autosense_in_descriptor_format() {
    let mut dev = ScriptedPipes::new();
    let sense = [0x72u8, 0x02, 0x3A, 0x00]; // NOT READY, MEDIUM NOT PRESENT
    dev.queue_sense(0x02, &sense);
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::WouldBlock));
    assert_eq!(pipes(&scsi).commands.len(), 1);
}

#[test]
fn a_foreign_tag_fails_the_exchange_closed() {
    let mut dev = ScriptedPipes::new();
    dev.echo_tag = false; // the scripted IU keeps its zero tag
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    assert_eq!(scsi.test_unit_ready(0), Err(Errno::BadMagic));
}

#[test]
fn a_ready_iu_of_the_wrong_direction_is_refused() {
    // Write Ready answering a read is a protocol violation.
    let mut dev = ScriptedPipes::new();
    dev.queue_ready(0x24);
    let mut scsi = scsi(dev);
    let mut buf = [0u8; 512];
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));

    // As is a second Read Ready after the data already moved.
    let mut dev = ScriptedPipes::new();
    dev.queue_ready(0x25);
    dev.data_in.push_back(vec![0u8; 512]);
    dev.queue_ready(0x25);
    let mut scsi = self::scsi(dev);
    assert_eq!(scsi.read(0, 0, 512, &mut buf), Err(Errno::BadMagic));
}

#[test]
fn a_sense_iu_lying_about_its_length_is_refused() {
    let mut dev = ScriptedPipes::new();
    let mut iu = vec![0u8; 16];
    iu[0] = 0x03;
    iu[14..16].copy_from_slice(&300u16.to_be_bytes()); // longer than the frame
    dev.status.push_back(iu);
    let mut scsi = scsi(dev);
    assert_eq!(scsi.test_unit_ready(0), Err(Errno::BadMagic));
}

#[test]
fn an_endless_status_stream_is_bounded_not_looped_on() {
    let mut dev = ScriptedPipes::new();
    for _ in 0..8 {
        dev.queue_ready(0x24);
    }
    let mut scsi = scsi(dev);
    let payload = [0u8; 512];
    // The first Write Ready moves the data; the second is refused, well
    // before the scripted queue runs dry.
    assert_eq!(scsi.write(0, 0, 512, &payload), Err(Errno::BadMagic));
    assert!(pipes(&scsi).status.len() >= 4);
}

#[test]
fn report_luns_discovers_the_unit_count() {
    let mut dev = ScriptedPipes::new();
    // Two single-level units: 0 and 2 → count 3 (the bring-up probes the
    // gap and skips it).
    dev.queue_ready(0x25);
    let mut payload = vec![0u8; 8 + 16];
    payload[0..4].copy_from_slice(&16u32.to_be_bytes());
    payload[8..16].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    payload[16..24].copy_from_slice(&[0, 2, 0, 0, 0, 0, 0, 0]);
    dev.data_in.push_back(payload);
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    assert_eq!(scsi.lun_count(), Ok(3));
    // The command was REPORT LUNS on LUN 0.
    let iu = &pipes(&scsi).commands[0];
    assert_eq!(iu[16], 0xA0);

    // A device that refuses the command serves its mandatory LUN 0, and
    // the refusal's sense never leaks into a later command's failure.
    let mut dev = ScriptedPipes::new();
    let mut sense = vec![0u8; 18];
    sense[0] = 0x70;
    sense[2] = 0x05;
    dev.queue_sense(0x02, &sense);
    let mut uas = Uas::new(dev);
    assert_eq!(uas.lun_count(), Ok(1));
    assert_eq!(uas.take_sense(), None);
}

#[test]
fn hierarchical_lun_entries_are_skipped_not_guessed() {
    let mut dev = ScriptedPipes::new();
    dev.queue_ready(0x25);
    let mut payload = vec![0u8; 8 + 16];
    payload[0..4].copy_from_slice(&16u32.to_be_bytes());
    // A hierarchical (multi-level) LUN and a plain unit 1.
    payload[8..16].copy_from_slice(&[0x40, 5, 0, 1, 0, 0, 0, 0]);
    payload[16..24].copy_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    dev.data_in.push_back(payload);
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    assert_eq!(scsi.lun_count(), Ok(2));
}

#[test]
fn tags_advance_per_command_and_skip_zero() {
    let mut dev = ScriptedPipes::new();
    dev.status.push_back(good());
    dev.status.push_back(good());
    let mut scsi = scsi(dev);
    assert_eq!(scsi.test_unit_ready(0), Ok(true));
    assert_eq!(scsi.test_unit_ready(0), Ok(true));
    let commands = &pipes(&scsi).commands;
    let tag0 = u16::from_be_bytes([commands[0][2], commands[0][3]]);
    let tag1 = u16::from_be_bytes([commands[1][2], commands[1][3]]);
    assert_ne!(tag0, 0);
    assert_ne!(tag1, 0);
    assert_ne!(tag0, tag1);
}
