//! The USB Attached SCSI (UAS) wire transport (`plans/DEVICES.md` D5).
//!
//! A UAS device (USB Mass Storage Class USB Attached SCSI Protocol,
//! protocol `0x62`, over T10 UAS) replaces BOT's one-command-at-a-time
//! CBW/CSW framing with four bulk pipes — command, status, data-in,
//! data-out, each named by a Pipe Usage descriptor — and typed Information
//! Units (IUs). The host sends a Command IU on the command pipe; the
//! device sequences the exchange from the status pipe: a Read/Write Ready
//! IU tells the host when to move the data pipes (USB 2.0 operation, where
//! no bulk streams exist to do that sequencing in hardware), and a Sense
//! IU carries the command's SAM status *and its sense data in-band* — UAS
//! has autosense; the command layer never issues `REQUEST SENSE`.
//!
//! [`Uas`] is that protocol as a [`ScsiTransport`] over the [`UasPipes`]
//! seam (the four pipes the `Run` binary maps onto endpoint-addressed URB
//! transfers), issuing one command at a time — the block service is
//! synchronous — with the tag of every returned IU checked against the
//! command in flight. The device is hostile input: an IU with the wrong
//! tag, id, shape, or ordering fails the exchange closed rather than
//! trusting the frame.
//!
//! Queueing multiple commands, task-management IUs (`ABORT TASK`), and
//! `SuperSpeed` bulk streams are deliberately not implemented yet: one
//! outstanding command needs none of them, a protocol violation surfaces
//! as a failed exchange, and the staged remainder is recorded in
//! `plans/DEVICES.md` §3.

use tairix_abi::Errno;

use crate::scsi::{CommandOutcome, DataPhase, ScsiTransport, Sense, MAX_LUNS};

/// The four UAS pipes (UAS §7): one logical transfer seam per pipe. The
/// `Run` binary implements it over the URB transport client, addressing
/// each pipe's own bulk endpoint; a host test implements it as a scripted
/// device.
pub trait UasPipes {
    /// Send one IU on the command pipe (bulk-OUT).
    ///
    /// # Errors
    ///
    /// An [`Errno`] from the transport/device.
    fn command_out(&mut self, iu: &[u8]) -> Result<(), Errno>;

    /// Read one IU from the status pipe (bulk-IN) into `buf`, returning
    /// the bytes delivered. Blocks until the device sends one.
    ///
    /// # Errors
    ///
    /// An [`Errno`] from the transport/device.
    fn status_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Read up to `buf.len()` data bytes from the data-in pipe, returning
    /// the bytes delivered (a short transfer ends the phase early).
    ///
    /// # Errors
    ///
    /// [`Errno::EndpointStalled`] for a device STALL (already recovered),
    /// or any other transport [`Errno`].
    fn data_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Write `buf` to the data-out pipe, returning the bytes the device
    /// accepted.
    ///
    /// # Errors
    ///
    /// As [`Self::data_in`].
    fn data_out(&mut self, buf: &[u8]) -> Result<usize, Errno>;

    /// Zero every byte of the transport's shared data window (the
    /// zero-on-free discipline applied to the bounce window).
    fn scrub(&mut self);
}

/// IU identifiers (UAS §6.2 table 4).
const IU_ID_COMMAND: u8 = 0x01;
const IU_ID_SENSE: u8 = 0x03;
const IU_ID_WRITE_READY: u8 = 0x24;
const IU_ID_READ_READY: u8 = 0x25;

/// Command IU length with a 16-byte CDB and no additional CDB bytes
/// (UAS §6.2.2).
pub const COMMAND_IU_LEN: usize = 32;

/// Sense IU header length: the sense data follows it (UAS §6.2.5).
pub const SENSE_IU_HEADER_LEN: usize = 16;

/// Most sense-data bytes accepted from a Sense IU. SPC-4 §4.5 bounds sense
/// data at 252 bytes; a longer claim is a protocol violation. A fixed
/// validation bound on hostile input, not a capacity.
const SENSE_DATA_MAX: usize = 252;

/// Buffer sized for the largest status-pipe IU this engine accepts.
const STATUS_IU_MAX: usize = SENSE_IU_HEADER_LEN + SENSE_DATA_MAX;

/// SAM-5 status codes carried by the Sense IU (§6.2.5).
const SAM_STATUS_GOOD: u8 = 0x00;

/// Most status-pipe IUs one command exchange may take: a ready IU, the
/// data phase, and the Sense IU — with one slot of tolerance. A device
/// still talking past this is violating the one-command protocol.
const MAX_STATUS_IUS: usize = 4;

/// One UAS device: the IU protocol over the four pipes, one command in
/// flight at a time.
pub struct Uas<T: UasPipes> {
    pipes: T,
    /// The tag of the next command; every returned IU must echo the tag of
    /// the command in flight. `0` is reserved by the protocol.
    next_tag: u16,
    /// Autosense captured from the last failed command's Sense IU.
    sense: Option<Sense>,
}

impl<T: UasPipes> Uas<T> {
    /// Wrap the four pipes of one UAS interface.
    pub fn new(pipes: T) -> Self {
        Self {
            pipes,
            next_tag: 1,
            sense: None,
        }
    }

    /// Borrow the underlying pipes (the serve loop observes
    /// transport-level state through them).
    #[must_use]
    pub fn pipes(&self) -> &T {
        &self.pipes
    }

    /// The tag for the next command exchange; never `0` (reserved).
    fn take_tag(&mut self) -> u16 {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.checked_add(1).unwrap_or(1);
        tag
    }

    /// Build the Command IU for `tag`/`lun`/`cdb` (UAS §6.2.2): a SIMPLE
    /// task attribute, no additional CDB bytes, and the single-level LUN
    /// encoding (SAM-5 §4.7 peripheral device addressing — the only form
    /// needed for the `< 256` unit numbers this driver serves).
    fn command_iu(tag: u16, lun: u8, cdb: &[u8]) -> [u8; COMMAND_IU_LEN] {
        let mut iu = [0u8; COMMAND_IU_LEN];
        iu[0] = IU_ID_COMMAND;
        iu[2..4].copy_from_slice(&tag.to_be_bytes());
        // Byte 4: priority/task attribute — SIMPLE (0). Byte 6: additional
        // CDB length in 4-byte units — 0 (a 16-byte CDB needs none).
        iu[9] = lun;
        iu[16..16 + cdb.len()].copy_from_slice(cdb);
        iu
    }

    /// Parse the sense data a Sense IU carries into the key/ASC/ASCQ
    /// triple, honouring both the fixed (SPC-4 §4.5.3) and descriptor
    /// (§4.5.2) formats. `None` for sense data too short or of an unknown
    /// format — never a fabricated triple.
    fn parse_sense(data: &[u8]) -> Option<Sense> {
        let response_code = data.first()? & 0x7F;
        match response_code {
            // Fixed format (current / deferred).
            0x70 | 0x71 => {
                if data.len() < 14 {
                    return None;
                }
                Some(Sense {
                    key: data[2] & 0x0F,
                    asc: data[12],
                    ascq: data[13],
                })
            }
            // Descriptor format (current / deferred).
            0x72 | 0x73 => {
                if data.len() < 4 {
                    return None;
                }
                Some(Sense {
                    key: data[1] & 0x0F,
                    asc: data[2],
                    ascq: data[3],
                })
            }
            _ => None,
        }
    }

    /// Read one status-pipe IU into `buf`, validating its shape and that
    /// it echoes `tag`.
    ///
    /// # Errors
    ///
    /// [`Errno::BadMagic`] for a frame too short to carry an IU header or
    /// carrying a foreign tag (a stale or forged IU is never matched to
    /// the command in flight), or a transport error.
    fn read_status_iu<'b>(&mut self, tag: u16, buf: &'b mut [u8]) -> Result<&'b [u8], Errno> {
        let received = self.pipes.status_in(buf)?;
        if received < 4 {
            return Err(Errno::BadMagic);
        }
        let iu = &buf[..received];
        let iu_tag = u16::from_be_bytes([iu[2], iu[3]]);
        if iu_tag != tag {
            return Err(Errno::BadMagic);
        }
        Ok(iu)
    }

    /// Interpret a Sense IU (UAS §6.2.5): capture the autosense for a
    /// CHECK CONDITION and report whether the command passed.
    ///
    /// # Errors
    ///
    /// [`Errno::BadMagic`] for a header too short or a sense length that
    /// lies about the frame (fail closed on hostile shape).
    fn finish_with_sense_iu(&mut self, iu: &[u8]) -> Result<bool, Errno> {
        if iu.len() < SENSE_IU_HEADER_LEN {
            return Err(Errno::BadMagic);
        }
        let status = iu[6];
        let sense_len = usize::from(u16::from_be_bytes([iu[14], iu[15]]));
        if sense_len > SENSE_DATA_MAX || SENSE_IU_HEADER_LEN + sense_len > iu.len() {
            return Err(Errno::BadMagic);
        }
        if status == SAM_STATUS_GOOD {
            return Ok(true);
        }
        // Any non-GOOD status is a failed command; a CHECK CONDITION
        // carries its explanation in-band. A busy/full status without
        // sense data simply leaves no autosense (the command layer then
        // asks the device, whose REQUEST SENSE travels this same path).
        self.sense = Self::parse_sense(&iu[SENSE_IU_HEADER_LEN..SENSE_IU_HEADER_LEN + sense_len]);
        Ok(false)
    }
}

impl<T: UasPipes> ScsiTransport for Uas<T> {
    /// Run one UAS command exchange: Command IU out, then the device's
    /// status-pipe sequencing — Read/Write Ready IU gating each data pipe
    /// (USB 2.0 non-stream operation), finished by the Sense IU.
    fn execute(
        &mut self,
        lun: u8,
        cb: &[u8],
        mut data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno> {
        if cb.is_empty() || cb.len() > 16 || lun as usize >= MAX_LUNS {
            return Err(Errno::OutOfRange);
        }
        // A stale capture must never explain a later command.
        self.sense = None;
        let tag = self.take_tag();
        let iu = Self::command_iu(tag, lun, cb);
        self.pipes.command_out(&iu)?;

        let mut transferred = 0usize;
        let mut data_moved = false;
        let mut status = [0u8; STATUS_IU_MAX];
        // Bounded: a well-behaved exchange is at most a ready IU plus the
        // Sense IU; a device that keeps talking is refused, never looped
        // on.
        for _ in 0..MAX_STATUS_IUS {
            let iu = self.read_status_iu(tag, &mut status)?;
            match iu[0] {
                IU_ID_READ_READY => {
                    let DataPhase::In(buf) = &mut data else {
                        return Err(Errno::BadMagic);
                    };
                    if data_moved || buf.is_empty() {
                        return Err(Errno::BadMagic);
                    }
                    match self.pipes.data_in(buf) {
                        Ok(n) => transferred = n,
                        // A stalled data pipe ends the phase; the Sense IU
                        // still delivers the verdict.
                        Err(Errno::EndpointStalled) => {}
                        Err(err) => return Err(err),
                    }
                    data_moved = true;
                }
                IU_ID_WRITE_READY => {
                    let DataPhase::Out(buf) = &data else {
                        return Err(Errno::BadMagic);
                    };
                    if data_moved || buf.is_empty() {
                        return Err(Errno::BadMagic);
                    }
                    match self.pipes.data_out(buf) {
                        Ok(n) => transferred = n,
                        Err(Errno::EndpointStalled) => {}
                        Err(err) => return Err(err),
                    }
                    data_moved = true;
                }
                IU_ID_SENSE => {
                    let passed = self.finish_with_sense_iu(iu)?;
                    return Ok(CommandOutcome {
                        passed,
                        transferred: transferred.min(data.len()),
                    });
                }
                // A Response IU (id `0x04`) answers task management, which
                // this engine never issues; it — and any other id — is not
                // a valid status-pipe IU for this exchange.
                _ => return Err(Errno::BadMagic),
            }
        }
        Err(Errno::BadMagic)
    }

    /// Number of logical units, via `REPORT LUNS` (SPC-4 §6.33; UAS
    /// mandates it). Only single-level peripheral-addressed units below
    /// [`MAX_LUNS`] are servable; the count is the highest such unit + 1
    /// (the bring-up path probes each and skips gaps). A device that
    /// refuses the command serves its mandatory LUN 0.
    fn lun_count(&mut self) -> Result<u8, Errno> {
        // Allocation: the 8-byte header plus one 8-byte entry per
        // addressable unit.
        const ALLOCATION: usize = 8 + MAX_LUNS * 8;
        let mut data = [0u8; ALLOCATION];
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0; // REPORT LUNS
        let alloc = u32::try_from(ALLOCATION).map_err(|_| Errno::LengthOutOfRange)?;
        cdb[6..10].copy_from_slice(&alloc.to_be_bytes());
        let outcome = self.execute(0, &cdb, DataPhase::In(&mut data))?;
        if !outcome.passed || outcome.transferred < 8 {
            // The command layer never sees this failure; drop the capture.
            self.sense = None;
            return Ok(1);
        }
        let list_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let available = outcome.transferred.saturating_sub(8).min(list_len);
        let mut highest = 0u8;
        for entry in data[8..8 + (available - available % 8)].as_chunks::<8>().0 {
            // Single-level peripheral addressing: byte 0 zero, byte 1 the
            // unit number, the remaining levels zero. Any other structure
            // is a hierarchical LUN this driver does not serve.
            if entry[0] != 0 || entry[2..] != [0u8; 6] {
                continue;
            }
            if usize::from(entry[1]) < MAX_LUNS {
                highest = highest.max(entry[1]);
            }
        }
        Ok(highest + 1)
    }

    /// UAS delivers sense in-band (autosense); hand over the last capture.
    fn take_sense(&mut self) -> Option<Sense> {
        self.sense.take()
    }

    fn scrub(&mut self) {
        self.pipes.scrub();
    }
}

#[cfg(test)]
#[path = "uas_tests.rs"]
mod tests;
