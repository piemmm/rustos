//! Shared-transport recovery coordination for the logical units of one
//! USB mass-storage device.
//!
//! A USB mass-storage device's logical units do **not** each own their own
//! transport: every LUN behind one Bulk-Only / UAS device shares the *same*
//! bulk pipe pair. A transport-wide event — a `CLEAR_FEATURE(HALT)` /
//! bulk-reset ([`ScsiDevice::scrub_window`](crate::scsi::ScsiDevice::scrub_window),
//! the driver's one data-path recovery mechanism), a port reset, a bus blip —
//! therefore hits *all* of them at once. Left uncoordinated, one LUN's stall
//! that triggers a shared-transport reset would look to every *other* LUN's
//! per-device [`BlkHealth`] machine like an independent failure, so a
//! multi-unit device (a card reader, a UAS enclosure) would report N spurious
//! disk faults for one transport blip.
//!
//! This module makes the shared transport a single **fault domain** over its
//! LUNs, reusing the arch-neutral [`FaultDomain`] primitive the interior
//! hub/controller nodes use (`plans/FIX-IO.md` IO4) rather than a second,
//! divergent rule. The serve loop opens one shared recovery window when it
//! resets the transport, holds every LUN's in-flight request reissuably under
//! that one window, and recovers the whole device the moment *any* unit
//! demonstrates the transport is back — so one blip is one recovery episode,
//! not N failures.
//!
//! The coordination is a pure, allocation-free function so it is proven
//! host-side over a fault-injecting [`Block`] double; the freestanding serve
//! loop in `program.rs` (metal-only — QEMU models no Pi USB) owns the timers
//! and the `lib/log` edges around it.

use tairix_abi::blkio::{
    decode_outcome, effective_child_status, serve_request_recovering, BlkCompletion, BlkHealth,
    BlkStatus, FaultDomain, FaultDomainState, BLK_COMPLETION_LEN,
};
use tairix_abi::driver::block::Block;

/// The mutable recovery state a serve loop threads through one LUN request: the
/// logical unit's own per-device [`BlkHealth`] machine and the device's shared
/// transport [`FaultDomain`].
///
/// The two are borrowed together because serving one request folds them
/// together (a per-device blip is ridden out by `health`; a shared-transport
/// blip is ridden out coherently across every unit by `domain`). The `health`
/// is per-LUN; the one `domain` is shared by all the device's units, so the
/// serve loop pairs each LUN's own `health` with that single shared `domain`
/// for the duration of the call.
pub struct LunRecovery<'a> {
    /// The logical unit's own per-device health machine.
    pub health: &'a mut BlkHealth,
    /// The device's shared-transport fault domain (one per device).
    pub domain: &'a mut FaultDomain,
}

/// Whether a device-level outcome proves the *shared transport* is alive
/// again.
///
/// The transport is demonstrably back only when the device produced a
/// **definitive** answer that required the command and its response to cross
/// the bulk pipes: it returned valid data ([`BlkStatus::data_valid`] —
/// `Ok`/`Degraded`) or reported a real medium sense
/// ([`BlkStatus::MediumError`], which is the *medium* faulting behind a
/// working transport, not the transport itself). A reissuable outcome
/// (`TransientError`/`Timeout`/`Reset`) means the transfer never landed, and a
/// gone outcome (`Offline`/`Removed`/`Fatal`) means the device is not
/// answering — neither demonstrates a return, so neither clears the shared
/// window (sticky-but-recoverable: only a demonstrated return recovers it).
#[must_use]
pub fn transport_alive(status: BlkStatus) -> bool {
    status.data_valid() || matches!(status, BlkStatus::MediumError)
}

/// Serve one LUN request through both its own per-device [`BlkHealth`] machine
/// and the device's shared-transport [`FaultDomain`], returning the framed
/// completion length written to `reply`.
///
/// The unit is **always driven** through the shared per-request engine
/// ([`serve_request_recovering`]): driving — rather than short-circuiting a
/// reissuable answer while the domain is recovering — is precisely what lets a
/// returning transport be *discovered*, since a definitive device answer is
/// the only demonstrated proof the shared pipes are back
/// ([`transport_alive`]). The device's own outcome is then folded with what
/// the shared transport imposes, in this order:
///
/// 1. **Detect a return first.** If the domain is not `Healthy` and the device
///    gave a definitive answer, the shared transport has demonstrably
///    returned: the domain recovers to `Healthy` so this very request is then
///    served on its own merits rather than being masked by the stale
///    recovering state.
/// 2. **Fold the domain's verdict.** [`effective_child_status`] combines the
///    device's status with what the (possibly just-recovered) transport
///    domain imposes. While the domain is `Recovering` a sibling LUN's request
///    is answered reissuably ([`BlkStatus::Reset`]) under the one shared
///    window; once that window has elapsed (or the domain is `Offline`) it is
///    failed closed ([`BlkStatus::Offline`]) — coherently across every unit.
///
/// The fold only ever *raises* severity over a **non-data-valid** device
/// status (the domain never overrides a fresh valid read, because such a read
/// recovers the domain in step 1), so re-framing with a default geometry never
/// discards data a consumer would have read. Framing cannot fail on the sized
/// `reply` buffer; a defensive fallback keeps the function panic-free.
#[must_use]
pub fn serve_lun_with_domain<B: Block>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
    reply: &mut [u8; BLK_COMPLETION_LEN],
    recovery: &mut LunRecovery<'_>,
    now_ns: u64,
) -> usize {
    let len = serve_request_recovering(
        device,
        read_only,
        request,
        window,
        reply,
        recovery.health,
        now_ns,
    );
    let device_status = decode_outcome(&reply[..len]).status;

    if recovery.domain.state() != FaultDomainState::Healthy && transport_alive(device_status) {
        recovery.domain.resume();
    }

    let effective =
        effective_child_status(device_status, core::iter::once(&*recovery.domain), now_ns);
    if effective == device_status {
        return len;
    }
    BlkCompletion::default()
        .encode_status(effective, reply)
        .unwrap_or(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use tairix_abi::blkio::{BlkDeviceClass, BlkOp, BlkRequest};
    use tairix_abi::driver::block::BlockGeometry;
    use tairix_abi::DriverError;

    const BLOCK_SIZE: usize = 512;
    /// `BLOCK_SIZE` as the wire-width type the geometry carries.
    const BLOCK_SIZE_U32: u32 = 512;
    const BLOCK_COUNT: u64 = 64;

    /// What one scripted read attempt does to the shared transport.
    #[derive(Copy, Clone, Debug)]
    enum Attempt {
        /// The transfer lands: valid data, transport up.
        Data,
        /// The bulk pipe stalls: a reissuable transport-level error.
        Stall,
        /// The medium reports a bad sector — the transport delivered the
        /// command and its sense, so the transport is up.
        Medium,
    }

    /// A [`Block`] double whose read outcomes are scripted per call, so a
    /// shared transport that stalls then returns can be modelled.
    struct ScriptedBlock {
        script: Vec<Attempt>,
        idx: usize,
    }

    impl ScriptedBlock {
        fn new(script: &[Attempt]) -> Self {
            Self {
                script: script.to_vec(),
                idx: 0,
            }
        }
    }

    impl Block for ScriptedBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, _lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let attempt = self.script.get(self.idx).copied().unwrap_or(Attempt::Data);
            self.idx += 1;
            match attempt {
                Attempt::Data => {
                    buf.fill(0xAB);
                    Ok(())
                }
                Attempt::Stall => Err(DriverError::EndpointStalled),
                Attempt::Medium => Err(DriverError::MediumError),
            }
        }

        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            Ok(())
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    fn read_one() -> [u8; tairix_abi::blkio::BLK_REQUEST_LEN] {
        let mut bytes = [0u8; tairix_abi::blkio::BLK_REQUEST_LEN];
        BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks: 1,
        }
        .encode(&mut bytes)
        .expect("encodes");
        bytes
    }

    /// The `Removable` class grace window this device's LUNs and their shared
    /// transport ride blips out under.
    fn grace_ns() -> u64 {
        BlkDeviceClass::Removable.budget().grace_ns
    }

    fn serve_read(
        device: &mut ScriptedBlock,
        health: &mut BlkHealth,
        domain: &mut FaultDomain,
        now_ns: u64,
    ) -> BlkStatus {
        let mut window = vec![0u8; BLOCK_SIZE];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut recovery = LunRecovery { health, domain };
        let len = serve_lun_with_domain(
            device,
            false,
            &read_one(),
            &mut window,
            &mut reply,
            &mut recovery,
            now_ns,
        );
        decode_outcome(&reply[..len]).status
    }

    #[test]
    fn transport_alive_is_exactly_the_definitive_answers() {
        assert!(transport_alive(BlkStatus::Ok));
        assert!(transport_alive(BlkStatus::Degraded));
        assert!(transport_alive(BlkStatus::MediumError));
        for gone_or_reissuable in [
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ] {
            assert!(!transport_alive(gone_or_reissuable));
        }
    }

    #[test]
    fn a_healthy_transport_passes_the_units_own_status_through() {
        let mut device = ScriptedBlock::new(&[Attempt::Data]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, 0),
            BlkStatus::Ok
        );
        assert_eq!(domain.state(), FaultDomainState::Healthy);
    }

    #[test]
    fn a_quiesced_transport_holds_a_stalling_sibling_reissuable_under_one_window() {
        // LUN A's stall triggered a shared-transport reset; the whole device
        // is now recovering. LUN B's request, whose transfer also stalls on
        // the still-wedged shared pipes, is answered reissuably under the one
        // shared window — not as an independent failure — and the window stays
        // open.
        let mut device = ScriptedBlock::new(&[Attempt::Stall]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        domain.quiesce(0);
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, grace_ns() / 2),
            BlkStatus::Reset
        );
        assert_eq!(domain.state(), FaultDomainState::Recovering);
    }

    #[test]
    fn any_units_definitive_answer_recovers_the_shared_transport() {
        // The shared transport is recovering; the moment any unit completes a
        // real transfer the transport is demonstrably back, so the whole
        // device recovers and this very request is served on its own merits.
        let mut device = ScriptedBlock::new(&[Attempt::Data]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        domain.quiesce(0);
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, grace_ns() / 2),
            BlkStatus::Ok
        );
        assert_eq!(domain.state(), FaultDomainState::Healthy);
    }

    #[test]
    fn a_bad_sector_recovers_the_transport_and_surfaces_the_medium_error() {
        // A medium error proves the transport delivered the command and its
        // sense, so it recovers the shared transport and is surfaced on its
        // own merits (never masked as a reissuable transport blip).
        let mut device = ScriptedBlock::new(&[Attempt::Medium]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        domain.quiesce(0);
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, grace_ns() / 2),
            BlkStatus::MediumError
        );
        assert_eq!(domain.state(), FaultDomainState::Healthy);
    }

    #[test]
    fn a_transport_window_that_elapses_fails_a_stalling_sibling_closed() {
        // The shared window elapsed with the transport still wedged: a
        // sibling's stalling request is failed closed to Offline, coherently
        // across the device, rather than held reissuable forever.
        let mut device = ScriptedBlock::new(&[Attempt::Stall]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        domain.quiesce(0);
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, grace_ns() + 1),
            BlkStatus::Offline
        );
    }

    #[test]
    fn a_returning_transport_after_the_window_elapsed_still_recovers() {
        // Sticky-but-recoverable: even after the shared window elapsed, a unit
        // that completes a real transfer demonstrates the transport is back
        // and recovers the whole device with no reboot.
        let mut device = ScriptedBlock::new(&[Attempt::Data]);
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let mut domain = FaultDomain::new(0, grace_ns());
        domain.quiesce(0);
        domain.poll(grace_ns() + 1);
        assert_eq!(domain.state(), FaultDomainState::Offline);
        assert_eq!(
            serve_read(&mut device, &mut health, &mut domain, grace_ns() + 2),
            BlkStatus::Ok
        );
        assert_eq!(domain.state(), FaultDomainState::Healthy);
    }
}
