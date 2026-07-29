//! The per-LUN block-service state machine: one decoded
//! [`BlkRequest`] in, one framed completion
//! out (`plans/DEVICES.md` D2).
//!
//! The `Run` binary receives each request on the LUN's call endpoint
//! (`call_recv`), hands it here with the LUN's shared data window, and
//! replies with the framed bytes (`call_reply`). This module is pure and
//! alloc-free, so the whole request surface — validation, the fail-closed
//! refusals, and the success paths — is proven host-side over an in-memory
//! [`Block`] double.
//!
//! The caller is untrusted: every request field is validated against the
//! live geometry and the mapped window before the device is touched, and a
//! request that cannot mean what it says is answered with an in-band error
//! completion, never a partial application.

use tairix_abi::blkio::{
    encode_error_completion, BlkCompletion, BlkHealth, BlkOp, BlkRequest, BlkStatus,
    BLK_COMPLETION_LEN, BLK_FLAG_READ_ONLY,
};
use tairix_abi::driver::block::Block;
use tairix_abi::{DriverError, Errno};

use crate::scsi::MAX_LUNS;

/// Reserved id range the per-LUN block-service endpoints are bound in
/// (`b"MSD\0"`-tagged, mirroring the HCD's URB endpoint range shape).
/// Each driver process derives one contiguous block of [`MAX_LUNS`] ids
/// from its URB endpoint grant ([`blk_block_for`]).
pub const BLK_ENDPOINT_BASE: u64 = 0x004D_5344_0000_0000;

/// Derive a driver process's block of [`MAX_LUNS`] contiguous
/// block-service endpoint ids from its URB endpoint grant: block base
/// `BLK_ENDPOINT_BASE | (grant counter × MAX_LUNS)`, LUN `n` at
/// `base + n`.
///
/// The kernel refuses to mint a second live endpoint with the grant's
/// id, so the counter in the grant's low half is unique among
/// concurrently served interfaces and the derived blocks are disjoint by
/// construction — a multi-drive enclosure's concurrently spawned driver
/// processes each create their endpoints first try, with no probing and
/// no rejected-create noise in the kernel log. Returns `None` when the
/// counter cannot be encoded inside the block id space (fail closed,
/// never a guessed or truncated id).
#[must_use]
pub fn blk_block_for(urb_endpoint: u64) -> Option<u64> {
    let stride = u64::try_from(MAX_LUNS).ok()?;
    let counter = urb_endpoint & 0xFFFF_FFFF;
    let offset = counter.checked_mul(stride)?;
    // The block (and its last LUN id) must stay inside the low 32-bit id
    // space beneath the `b"MSD\0"` tag.
    if offset + (stride - 1) > u64::from(u32::MAX) {
        return None;
    }
    Some(BLK_ENDPOINT_BASE | offset)
}

/// One validated request's outcome, kept separate at the source so device
/// health is only ever driven by the *device's* own behaviour. A request the
/// driver refuses up front, or that the device rejects as out-of-range, is a
/// [`Served::Refused`] the recovery arm frames verbatim without ever counting
/// it against the grace window — a hostile or malformed request can never
/// drive a healthy device toward [`tairix_abi::blkio::BlkHealthState::Faulted`].
enum Served {
    /// The request was validated and handed to the device; this is what the
    /// device call returned (its [`DriverError`] carries the health signal).
    Device(Result<BlkCompletion, DriverError>),
    /// The request itself was refused (malformed, read-only, or larger than
    /// the window). Health-neutral.
    Refused(Errno),
}

/// A data request's extent could not be resolved: either the request was
/// refused (health-neutral) or the device's own geometry call failed
/// (device-level, and so health-relevant).
enum Extent {
    Refused(Errno),
    Device(DriverError),
}

/// Serve one block-service request over `device` and the LUN's shared data
/// `window`, folding the device's outcome into `health` and framing the
/// resulting completion into `reply`, returning its length.
///
/// `read_only` is the LUN's write policy (the MODE SENSE WP bit); a write
/// against it is refused before the device is touched, and the geometry reply
/// carries it as [`BLK_FLAG_READ_ONLY`] so a consumer can present the volume
/// honestly.
///
/// `now_ns` is the monotonic clock reading (the kernel monotonic clock the
/// driver reads before serving each request) that times the recovery grace
/// window.
///
/// A device-level transient stall inside the
/// window is answered with a reissuable [`BlkStatus::Reset`]; the same stall
/// after the window elapses is failed closed as [`BlkStatus::Offline`]. A
/// valid answer recovers the device. A request-level refusal is framed
/// verbatim and never touches `health`, so head-of-line freedom holds: the
/// serve loop never parks on one device's blip.
///
/// Framing a completion cannot fail (the buffer is exactly the sized
/// destination); a defensive `unwrap_or(0)` keeps this panic-free on any path
/// rather than relying on that invariant.
pub fn serve_request_recovering<B: Block>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
    reply: &mut [u8; BLK_COMPLETION_LEN],
    health: &mut BlkHealth,
    now_ns: u64,
) -> usize {
    match classify(device, read_only, request, window) {
        Served::Refused(err) => encode_error_completion(reply, err).unwrap_or(0),
        Served::Device(Ok(completion)) => {
            let status = health.observe(BlkStatus::Ok, now_ns);
            completion.encode_status(status, reply).unwrap_or(0)
        }
        Served::Device(Err(err)) => match BlkStatus::for_driver_health(err) {
            // A device-health error drives the recovery state machine: the
            // status the consumer is told may be softened to reissuable while
            // inside the grace window or hardened to offline once it elapses.
            Some(raw) => {
                let status = health.observe(raw, now_ns);
                BlkCompletion::default()
                    .encode_status(status, reply)
                    .unwrap_or(0)
            }
            // A request-level rejection the device raised (an out-of-range
            // LBA) is not about the device's health: frame it verbatim.
            None => encode_error_completion(reply, err.as_errno()).unwrap_or(0),
        },
    }
}

/// Decode, validate, and execute one request on `device`, classifying the
/// outcome as a device call or a health-neutral request refusal.
fn classify<B: Block>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
) -> Served {
    let request = match BlkRequest::decode(request) {
        Ok(request) => request,
        Err(err) => return Served::Refused(err),
    };
    match request.op {
        BlkOp::Geometry => Served::Device(device.geometry().map(|geometry| BlkCompletion {
            block_size: geometry.block_size,
            block_count: geometry.block_count,
            flags: if read_only { BLK_FLAG_READ_ONLY } else { 0 },
        })),
        BlkOp::Read => match data_extent(device, request.blocks, window.len()) {
            Ok(len) => Served::Device(
                device
                    .read_blocks(request.lba, &mut window[..len])
                    .map(|()| BlkCompletion::default()),
            ),
            Err(Extent::Refused(err)) => Served::Refused(err),
            Err(Extent::Device(err)) => Served::Device(Err(err)),
        },
        BlkOp::Write => {
            // The write policy is enforced here as well as in the Block
            // implementation below, so no request shape can route around it.
            if read_only {
                return Served::Refused(Errno::PermissionDenied);
            }
            match data_extent(device, request.blocks, window.len()) {
                Ok(len) => Served::Device(
                    device
                        .write_blocks(request.lba, &window[..len])
                        .map(|()| BlkCompletion::default()),
                ),
                Err(Extent::Refused(err)) => Served::Refused(err),
                Err(Extent::Device(err)) => Served::Device(Err(err)),
            }
        }
        BlkOp::Flush => Served::Device(device.flush().map(|()| BlkCompletion::default())),
    }
}

/// Byte length a data request covers, validated against the geometry and the
/// mapped window (the device's own range check still applies inside the
/// [`Block`] call). A zero-block or oversized request is a health-neutral
/// refusal; a failing geometry call is a device-level fault.
fn data_extent<B: Block>(device: &B, blocks: u32, window_len: usize) -> Result<usize, Extent> {
    if blocks == 0 {
        return Err(Extent::Refused(Errno::OutOfRange));
    }
    let geometry = device.geometry().map_err(Extent::Device)?;
    let len = (blocks as usize)
        .checked_mul(geometry.block_size as usize)
        .ok_or(Extent::Refused(Errno::LengthOutOfRange))?;
    if len > window_len {
        return Err(Extent::Refused(Errno::LengthOutOfRange));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use tairix_abi::blkio::{
        decode_completion, decode_outcome, BlkDeviceClass, BlkHealthState, BLK_DATA_LEN,
        BLK_REQUEST_LEN,
    };
    use tairix_abi::driver::block::BlockGeometry;

    #[test]
    fn blk_blocks_derived_from_distinct_grants_never_overlap() {
        // The Pi 4 metal defect: ten concurrently spawned driver
        // processes (a multi-drive enclosure binds one per bridge) probed
        // a shared id range for a free block, and every taken probe
        // logged a kernel-side rejected `call_create`. The block is now
        // derived from the URB endpoint grant, whose counter the kernel
        // guarantees unique among live interfaces: distinct grants yield
        // disjoint MAX_LUNS-sized blocks, so every create succeeds first
        // try.
        let tag = 0x0055_5242_0000_0000u64;
        let mut previous_end = 0u64;
        for counter in [0u64, 1, 2, 9, 10, 0x1000, 0xFFF_FFFE] {
            let base = blk_block_for(tag | counter).expect("encodable counter derives");
            assert_eq!(base & 0xFFFF_FFFF_0000_0000, BLK_ENDPOINT_BASE);
            assert!(
                counter == 0 || base >= previous_end,
                "blocks of increasing counters never overlap"
            );
            previous_end = base + u64::try_from(MAX_LUNS).expect("small");
        }
    }

    #[test]
    fn an_unencodable_grant_counter_fails_the_derivation_closed() {
        // A counter whose block would spill past the 32-bit id space
        // under the b"MSD\0" tag is refused, never wrapped or truncated
        // into a colliding id.
        assert_eq!(blk_block_for(0x0055_5242_FFFF_FFFF), None);
        assert_eq!(blk_block_for(0x0055_5242_1000_0000), None);
    }

    /// An in-memory 512-byte-block device with a flush counter.
    struct MemBlock {
        data: Vec<u8>,
        flushes: usize,
    }

    const BLOCK_SIZE: usize = 512;
    const BLOCK_COUNT: u64 = 64;
    /// `BLOCK_SIZE` as the wire-width type the geometry carries.
    const BLOCK_SIZE_U32: u32 = 512;

    impl MemBlock {
        fn new() -> Self {
            Self {
                data: vec![0u8; BLOCK_SIZE * 64],
                flushes: 0,
            }
        }
    }

    impl Block for MemBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let start =
                usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::LengthOutOfRange)?;
            if !buf.len().is_multiple_of(BLOCK_SIZE) || end > self.data.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            buf.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let start =
                usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::LengthOutOfRange)?;
            if !buf.len().is_multiple_of(BLOCK_SIZE) || end > self.data.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.data[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn encode(request: &BlkRequest) -> [u8; BLK_REQUEST_LEN] {
        let mut bytes = [0u8; BLK_REQUEST_LEN];
        request.encode(&mut bytes).expect("encodes");
        bytes
    }

    /// Serve one request against a fresh-`Healthy` device at time zero: the
    /// success and request-refusal paths these tests exercise are independent
    /// of the recovery state, so the health tracking is transparent here (its
    /// own transitions are proven by the recovery tests below).
    fn serve<B: Block>(
        device: &mut B,
        read_only: bool,
        request: &BlkRequest,
        window: &mut [u8],
    ) -> Result<BlkCompletion, Errno> {
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let len = serve_request_recovering(
            device,
            read_only,
            &encode(request),
            window,
            &mut reply,
            &mut health,
            0,
        );
        decode_completion(&reply[..len])
    }

    #[test]
    fn geometry_reports_the_device_and_the_write_policy() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let request = BlkRequest {
            op: BlkOp::Geometry,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &request, &mut window),
            Ok(BlkCompletion {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
                flags: 0,
            })
        );
        assert_eq!(
            serve(&mut device, true, &request, &mut window),
            Ok(BlkCompletion {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
                flags: BLK_FLAG_READ_ONLY,
            })
        );
    }

    #[test]
    fn write_then_read_round_trips_through_the_window() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        window[..BLOCK_SIZE].copy_from_slice(&[0xA5u8; BLOCK_SIZE]);
        let write = BlkRequest {
            op: BlkOp::Write,
            lba: 3,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &write, &mut window),
            Ok(BlkCompletion::default())
        );

        window.fill(0);
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 3,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Ok(BlkCompletion::default())
        );
        assert_eq!(&window[..BLOCK_SIZE], &[0xA5u8; BLOCK_SIZE]);
    }

    #[test]
    fn a_write_to_a_read_only_lun_is_refused_before_the_device() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        window[..BLOCK_SIZE].fill(0xFF);
        let write = BlkRequest {
            op: BlkOp::Write,
            lba: 0,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, true, &write, &mut window),
            Err(Errno::PermissionDenied)
        );
        // Nothing reached the medium.
        assert!(device.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_transfer_larger_than_the_window_is_refused() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let blocks = u32::try_from(BLK_DATA_LEN / BLOCK_SIZE + 1).expect("fits");
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_zero_block_transfer_is_refused() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn an_out_of_range_read_surfaces_the_device_refusal() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: BLOCK_COUNT,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_malformed_request_is_answered_in_band() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let len = serve_request_recovering(
            &mut device,
            false,
            &[0u8; BLK_REQUEST_LEN - 1],
            &mut window,
            &mut reply,
            &mut health,
            0,
        );
        assert_eq!(
            decode_completion(&reply[..len]),
            Err(Errno::LengthOutOfRange)
        );
        // A malformed request never counts against the device's health.
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn flush_reaches_the_device_exactly_once() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let flush = BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &flush, &mut window),
            Ok(BlkCompletion::default())
        );
        assert_eq!(device.flushes, 1);
    }

    /// A device that answers geometry from a fixed [`BlockGeometry`] but
    /// injects a chosen [`DriverError`] into every data transfer — a
    /// stand-in for a disk that is stalling, has a bad sector, or has gone
    /// offline. `fault = None` means it serves reads normally.
    struct FaultyBlock {
        fault: Option<DriverError>,
    }

    impl Block for FaultyBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, _lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            // A healthy read serves deterministic zeroes; an injected fault is
            // returned as the device's own error.
            if let Some(err) = self.fault {
                return Err(err);
            }
            buf.fill(0);
            Ok(())
        }

        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            self.fault.map_or(Ok(()), Err)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// Serve one read against `device`, returning the framed completion's
    /// [`BlkStatus`] so a test can assert the health axis the consumer sees.
    fn serve_read_status(
        device: &mut FaultyBlock,
        health: &mut BlkHealth,
        now_ns: u64,
    ) -> BlkStatus {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let read = encode(&BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks: 1,
        });
        let len = serve_request_recovering(
            device,
            false,
            &read,
            &mut window,
            &mut reply,
            health,
            now_ns,
        );
        decode_outcome(&reply[..len]).status
    }

    #[test]
    fn a_transient_stall_is_ridden_out_then_fails_closed_at_the_window() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::EndpointStalled),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let grace = health.budget().grace_ns;
        // Inside the grace window the stall is answered reissuably, and the
        // device is held Recovering — never hard-failed on the first blip.
        assert_eq!(
            serve_read_status(&mut device, &mut health, 0),
            BlkStatus::Reset
        );
        assert_eq!(health.state(), BlkHealthState::Recovering);
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace / 2),
            BlkStatus::Reset
        );
        // Still stalling once the window elapses: only now is it failed closed.
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace),
            BlkStatus::Offline
        );
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // The device comes back: the next good read recovers it, no reboot.
        device.fault = None;
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace + 1),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_blip_that_returns_inside_the_window_leaves_no_scar() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::Busy),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(
            serve_read_status(&mut device, &mut health, 10),
            BlkStatus::Reset
        );
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // Returns well inside the window: fully recovered, invisible to data.
        device.fault = None;
        assert_eq!(
            serve_read_status(&mut device, &mut health, 100),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_bad_sector_surfaces_without_faulting_the_device() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::MediumError),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::Rotational);
        // A permanent medium error is surfaced to the request, but the device
        // itself stays Healthy — a bad block is not a dead disk.
        assert_eq!(
            serve_read_status(&mut device, &mut health, 0),
            BlkStatus::MediumError
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_device_out_of_range_rejection_is_health_neutral() {
        // The device rejects an out-of-range LBA (a request-level fault it
        // raised): it must not count against the grace window.
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        let read = encode(&BlkRequest {
            op: BlkOp::Read,
            lba: BLOCK_COUNT,
            blocks: 1,
        });
        let len = serve_request_recovering(
            &mut device,
            false,
            &read,
            &mut window,
            &mut reply,
            &mut health,
            0,
        );
        assert_eq!(
            decode_completion(&reply[..len]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }
}
