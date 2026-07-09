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

use rustos_abi::blkio::{
    encode_error_completion, BlkCompletion, BlkOp, BlkRequest, BLK_COMPLETION_LEN,
    BLK_FLAG_READ_ONLY,
};
use rustos_abi::driver::block::Block;
use rustos_abi::{DriverError, Errno};

use crate::bot::Flush;

/// Serve one block-service request over `device` and the LUN's shared
/// data `window`, framing the completion into `reply` and returning its
/// length.
///
/// `read_only` is the LUN's write policy (the MODE SENSE WP bit); a write
/// against it is refused before the device is touched, and the geometry
/// reply carries it as [`BLK_FLAG_READ_ONLY`] so a consumer can present
/// the volume honestly.
///
/// Framing a completion cannot fail (the buffer is exactly the sized
/// destination); a defensive `unwrap_or(0)` keeps this panic-free on any
/// path rather than relying on that invariant.
pub fn serve_request<B: Block + Flush>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
    reply: &mut [u8; BLK_COMPLETION_LEN],
) -> usize {
    let result = serve_decoded(device, read_only, request, window);
    match result {
        Ok(completion) => completion.encode(reply).unwrap_or(0),
        Err(err) => encode_error_completion(reply, err).unwrap_or(0),
    }
}

/// The request body behind [`serve_request`]: decode, validate, execute.
fn serve_decoded<B: Block + Flush>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
) -> Result<BlkCompletion, Errno> {
    let request = BlkRequest::decode(request)?;
    match request.op {
        BlkOp::Geometry => {
            let geometry = device.geometry().map_err(DriverError::as_errno)?;
            Ok(BlkCompletion {
                block_size: geometry.block_size,
                block_count: geometry.block_count,
                flags: if read_only { BLK_FLAG_READ_ONLY } else { 0 },
            })
        }
        BlkOp::Read => {
            let len = transfer_len(device, request.blocks, window.len())?;
            device
                .read_blocks(request.lba, &mut window[..len])
                .map_err(DriverError::as_errno)?;
            Ok(BlkCompletion::default())
        }
        BlkOp::Write => {
            // The write policy is enforced here as well as in the Block
            // implementation below, so no request shape can route around
            // it.
            if read_only {
                return Err(Errno::PermissionDenied);
            }
            let len = transfer_len(device, request.blocks, window.len())?;
            device
                .write_blocks(request.lba, &window[..len])
                .map_err(DriverError::as_errno)?;
            Ok(BlkCompletion::default())
        }
        BlkOp::Flush => {
            device.flush().map_err(DriverError::as_errno)?;
            Ok(BlkCompletion::default())
        }
    }
}

/// Byte length a data request covers, validated against the geometry and
/// the mapped window (the device's own range check still applies inside
/// the [`Block`] call).
fn transfer_len<B: Block>(device: &B, blocks: u32, window_len: usize) -> Result<usize, Errno> {
    if blocks == 0 {
        return Err(Errno::OutOfRange);
    }
    let geometry = device.geometry().map_err(DriverError::as_errno)?;
    let len = (blocks as usize)
        .checked_mul(geometry.block_size as usize)
        .ok_or(Errno::LengthOutOfRange)?;
    if len > window_len {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use rustos_abi::blkio::{decode_completion, BLK_DATA_LEN, BLK_REQUEST_LEN};
    use rustos_abi::driver::block::BlockGeometry;

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
            if buf.len() % BLOCK_SIZE != 0 || end > self.data.len() {
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
            if buf.len() % BLOCK_SIZE != 0 || end > self.data.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.data[start..end].copy_from_slice(buf);
            Ok(())
        }
    }

    impl Flush for MemBlock {
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

    fn serve(
        device: &mut MemBlock,
        read_only: bool,
        request: &BlkRequest,
        window: &mut [u8],
    ) -> Result<BlkCompletion, Errno> {
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let len = serve_request(device, read_only, &encode(request), window, &mut reply);
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
        let len = serve_request(
            &mut device,
            false,
            &[0u8; BLK_REQUEST_LEN - 1],
            &mut window,
            &mut reply,
        );
        assert_eq!(
            decode_completion(&reply[..len]),
            Err(Errno::LengthOutOfRange)
        );
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
}
