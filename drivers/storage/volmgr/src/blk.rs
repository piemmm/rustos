//! The volume manager's blkio block client: `tairix_abi::driver::block::Block`
//! over the block-service call endpoint + shared data window its matched
//! storage node granted.
//!
//! The wire protocol is the fixed-frame `tairix_abi::blkio` request/reply
//! pair; data moves through the shared window the serving block driver
//! created. The transport call lives behind the [`BlkCall`] seam so the
//! client is host-testable without a kernel: each call receives the
//! client's mapping of the shared window, which the production transport
//! ignores (the serving driver fills the same frames through its own
//! mapping during the `ipc_call`) and a host test double fills directly,
//! playing the serving driver.
//!
//! Everything the device reports is untrusted: the geometry is validated
//! at [`RemoteBlock::connect`] before any consumer sees it, every reply
//! frame is decoded fail-closed, and a transfer never reads more bytes out
//! of the window than the request named. The client is deliberately
//! **read-only**: the probe writes nothing, so `write_blocks` refuses
//! rather than carrying authority the volume manager does not need.

use tairix_abi::blkio::{
    BlkCompletion, BlkOp, BlkRequest, BLK_COMPLETION_LEN, BLK_DATA_LEN, BLK_FLAG_READ_ONLY,
    BLK_REQUEST_LEN,
};
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::{DriverError, Errno};

/// Smallest logical block size a sane device reports.
const MIN_BLOCK_SIZE: u32 = 512;

/// Largest logical block size the shared data window is sized for.
const MAX_BLOCK_SIZE: u32 = 4096;

/// One synchronous request/reply exchange with the serving block driver.
///
/// `window` is the client's mapping of the shared data window. The
/// production implementation issues `ipc_call` on the granted endpoint and
/// never touches `window` itself — the serving driver moves the data
/// through its own mapping of the same frames — while a host test double
/// writes `window` directly to play the serving driver. The kernel
/// re-checks the caller's endpoint grant and capability on every call;
/// this seam adds no authority.
pub trait BlkCall {
    /// Send `request`, receive the completion into `reply`, and return the
    /// reply length.
    ///
    /// # Errors
    ///
    /// The transport's [`Errno`] (e.g. a vanished endpoint).
    fn call(&mut self, request: &[u8], reply: &mut [u8], window: &mut [u8])
        -> Result<usize, Errno>;
}

/// A read-only [`Block`] view of one served logical unit.
pub struct RemoteBlock<'w, C: BlkCall> {
    call: C,
    /// This process's mapping of the shared data window the serving
    /// driver created (its length bounds every transfer chunk).
    window: &'w mut [u8],
    geometry: BlockGeometry,
    read_only: bool,
}

impl<'w, C: BlkCall> RemoteBlock<'w, C> {
    /// Connect the client: query the device geometry and validate it
    /// before any consumer can issue a transfer.
    ///
    /// # Errors
    ///
    /// * The transport's [`Errno`] when the geometry call fails.
    /// * [`Errno::OutOfRange`] for a hostile geometry (block size not a
    ///   power of two in 512..=4096, a zero block count, or a byte
    ///   capacity that overflows) or a window too small to hold one block.
    pub fn connect(call: C, window: &'w mut [u8]) -> Result<Self, Errno> {
        let mut client = Self {
            call,
            window,
            geometry: BlockGeometry {
                block_size: 0,
                block_count: 0,
            },
            read_only: false,
        };
        let completion = client.transfer(BlkRequest {
            op: BlkOp::Geometry,
            lba: 0,
            blocks: 0,
        })?;
        let block_size = completion.block_size;
        if !block_size.is_power_of_two() || !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size)
        {
            return Err(Errno::OutOfRange);
        }
        if completion.block_count == 0
            || completion
                .block_count
                .checked_mul(u64::from(block_size))
                .is_none()
        {
            return Err(Errno::OutOfRange);
        }
        if client.window.len() < block_size as usize {
            return Err(Errno::OutOfRange);
        }
        client.geometry = BlockGeometry {
            block_size,
            block_count: completion.block_count,
        };
        client.read_only = completion.flags & BLK_FLAG_READ_ONLY != 0;
        Ok(client)
    }

    /// Whether the device reported itself write-protected.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Issue one request and decode its completion fail-closed.
    fn transfer(&mut self, request: BlkRequest) -> Result<BlkCompletion, Errno> {
        let mut frame = [0u8; BLK_REQUEST_LEN];
        let len = request.encode(&mut frame)?;
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let got = self.call.call(&frame[..len], &mut reply, self.window)?;
        tairix_abi::blkio::decode_completion(reply.get(..got).ok_or(Errno::BadMagic)?)
    }

    /// Largest whole-block chunk a single transfer can move through the
    /// window, in blocks (a `usize`: it is bounded by the window length).
    fn blocks_per_chunk(&self) -> usize {
        let window = self.window.len().min(BLK_DATA_LEN);
        window / self.geometry.block_size as usize
    }
}

impl<C: BlkCall> Block for RemoteBlock<'_, C> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let block_size = self.geometry.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(block_size) {
            return Err(DriverError::OutOfRange);
        }
        let blocks = buf.len() / block_size;
        let end = lba
            .checked_add(blocks as u64)
            .ok_or(DriverError::OutOfRange)?;
        if end > self.geometry.block_count {
            return Err(DriverError::OutOfRange);
        }
        // Chunk arithmetic stays in `usize` (both operands derive from
        // buffer lengths), so no width-truncating cast exists on any
        // pointer width.
        let mut done = 0usize;
        while done < blocks {
            let chunk_blocks = (blocks - done).min(self.blocks_per_chunk());
            let request = BlkRequest {
                op: BlkOp::Read,
                lba: lba + done as u64,
                blocks: u32::try_from(chunk_blocks).map_err(|_| DriverError::OutOfRange)?,
            };
            self.transfer(request).map_err(errno_to_driver)?;
            let chunk_bytes = chunk_blocks * block_size;
            let offset = done * block_size;
            buf[offset..offset + chunk_bytes].copy_from_slice(&self.window[..chunk_bytes]);
            done += chunk_blocks;
        }
        Ok(())
    }

    fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
        // The probe is a pure reader; refusing here keeps the client's
        // authority as small as its job (the kernel's own attach-time
        // client performs any writing the mounted filesystem needs).
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // A pure reader never issues a write, so there is nothing the
        // device could hold uncommitted on this client's behalf. The
        // attach-time write client (which does write) owns the durability
        // flush; here it is a truthful no-op, not a swallowed forward.
        Ok(())
    }
}

/// Map a transport/completion [`Errno`] onto the block-driver error the
/// `Block` consumer expects. Unknown codes fail closed as a device fault.
fn errno_to_driver(err: Errno) -> DriverError {
    match err {
        Errno::PermissionDenied => DriverError::PermissionDenied,
        Errno::OutOfRange | Errno::LengthOutOfRange => DriverError::OutOfRange,
        Errno::NotFound => DriverError::NotFound,
        _ => DriverError::DeviceFault,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    /// A scripted serving driver: a device whose block content is a
    /// deterministic function of the byte address, filled straight into
    /// the shared window exactly as the real driver would.
    struct MemDevice {
        block_size: u32,
        block_count: u64,
        flags: u32,
        calls: Vec<BlkRequest>,
    }

    fn fill(buf: &mut [u8], byte_base: u64) {
        for (i, out) in buf.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *out = ((byte_base + i as u64) % 251) as u8;
            }
        }
    }

    impl BlkCall for MemDevice {
        fn call(
            &mut self,
            request: &[u8],
            reply: &mut [u8],
            window: &mut [u8],
        ) -> Result<usize, Errno> {
            let decoded = BlkRequest::decode(request)?;
            self.calls.push(decoded);
            match decoded.op {
                BlkOp::Geometry => BlkCompletion {
                    block_size: self.block_size,
                    block_count: self.block_count,
                    flags: self.flags,
                }
                .encode(reply),
                BlkOp::Read => {
                    let bytes = decoded.blocks as usize * self.block_size as usize;
                    if bytes > window.len() {
                        return tairix_abi::blkio::encode_error_completion(
                            reply,
                            Errno::LengthOutOfRange,
                        );
                    }
                    fill(
                        &mut window[..bytes],
                        decoded.lba * u64::from(self.block_size),
                    );
                    BlkCompletion::default().encode(reply)
                }
                _ => tairix_abi::blkio::encode_error_completion(reply, Errno::PermissionDenied),
            }
        }
    }

    fn device(block_size: u32, block_count: u64, flags: u32) -> MemDevice {
        MemDevice {
            block_size,
            block_count,
            flags,
            calls: Vec::new(),
        }
    }

    #[test]
    fn connect_validates_geometry_and_read_only_flag() {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let client = RemoteBlock::connect(device(512, 64, BLK_FLAG_READ_ONLY), &mut window)
            .expect("connects");
        assert!(client.read_only());
        assert_eq!(
            client.geometry().expect("geometry"),
            BlockGeometry {
                block_size: 512,
                block_count: 64
            },
            "the validated geometry is cached"
        );
    }

    #[test]
    fn hostile_geometries_are_refused_at_connect() {
        for (block_size, block_count) in [
            (0u32, 64u64),
            (513, 64),
            (256, 64),
            (8192, 64),
            (512, 0),
            (4096, u64::MAX),
        ] {
            let mut window = vec![0u8; BLK_DATA_LEN];
            assert!(
                RemoteBlock::connect(device(block_size, block_count, 0), &mut window).is_err(),
                "{block_size}x{block_count} must be refused"
            );
        }
    }

    #[test]
    fn a_window_smaller_than_one_block_is_refused() {
        let mut window = vec![0u8; 511];
        assert_eq!(
            RemoteBlock::connect(device(512, 64, 0), &mut window).err(),
            Some(Errno::OutOfRange)
        );
    }

    #[test]
    fn reads_chunk_through_the_window_and_preserve_data() {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect(device(512, 1024, 0), &mut window).expect("connects");

        // Read a span larger than one window chunk so it must split.
        let blocks = (BLK_DATA_LEN / 512) as u64 + 3;
        let mut buf = vec![0u8; usize::try_from(blocks).expect("fits") * 512];
        client.read_blocks(5, &mut buf).expect("reads");

        let mut expected = vec![0u8; buf.len()];
        fill(&mut expected, 5 * 512);
        assert_eq!(buf, expected, "chunked data arrives in order, intact");

        // Geometry + two read chunks were issued.
        let reads: Vec<_> = client
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Read)
            .collect();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].lba, 5);
        assert_eq!(reads[1].lba, 5 + (BLK_DATA_LEN / 512) as u64);
    }

    #[test]
    fn shape_and_extent_violations_fail_before_any_request() {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect(device(512, 8, 0), &mut window).expect("connects");

        let mut misaligned = [0u8; 100];
        assert_eq!(
            client.read_blocks(0, &mut misaligned),
            Err(DriverError::OutOfRange)
        );
        let mut empty: [u8; 0] = [];
        assert_eq!(
            client.read_blocks(0, &mut empty),
            Err(DriverError::OutOfRange)
        );
        let mut past_end = [0u8; 512];
        assert_eq!(
            client.read_blocks(8, &mut past_end),
            Err(DriverError::OutOfRange)
        );
        assert_eq!(
            client.read_blocks(u64::MAX, &mut past_end),
            Err(DriverError::OutOfRange)
        );
        let reads = client.call.calls.iter().filter(|r| r.op == BlkOp::Read);
        assert_eq!(reads.count(), 0, "no invalid request reached the wire");
    }

    #[test]
    fn writes_are_refused_client_side() {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect(device(512, 8, 0), &mut window).expect("connects");
        assert_eq!(
            client.write_blocks(0, &[0u8; 512]),
            Err(DriverError::Unsupported)
        );
        assert_eq!(
            client
                .call
                .calls
                .iter()
                .filter(|r| r.op == BlkOp::Write)
                .count(),
            0,
            "no write reached the wire"
        );
    }

    #[test]
    fn an_error_completion_surfaces_as_a_typed_fault() {
        /// A device that refuses every read with a permission error.
        struct Refusing;
        impl BlkCall for Refusing {
            fn call(
                &mut self,
                request: &[u8],
                reply: &mut [u8],
                _window: &mut [u8],
            ) -> Result<usize, Errno> {
                if BlkRequest::decode(request)?.op == BlkOp::Geometry {
                    BlkCompletion {
                        block_size: 512,
                        block_count: 8,
                        flags: 0,
                    }
                    .encode(reply)
                } else {
                    tairix_abi::blkio::encode_error_completion(reply, Errno::PermissionDenied)
                }
            }
        }
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect(Refusing, &mut window).expect("connects");
        let mut buf = [0u8; 512];
        assert_eq!(
            client.read_blocks(0, &mut buf),
            Err(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn a_truncated_or_corrupt_reply_fails_closed() {
        /// A device that replies with a truncated success frame.
        struct Truncating;
        impl BlkCall for Truncating {
            fn call(
                &mut self,
                request: &[u8],
                reply: &mut [u8],
                _window: &mut [u8],
            ) -> Result<usize, Errno> {
                if BlkRequest::decode(request)?.op == BlkOp::Geometry {
                    BlkCompletion {
                        block_size: 512,
                        block_count: 8,
                        flags: 0,
                    }
                    .encode(reply)
                } else {
                    reply[..4].copy_from_slice(&0i32.to_le_bytes());
                    Ok(4) // success status with the payload missing
                }
            }
        }
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect(Truncating, &mut window).expect("connects");
        let mut buf = [0u8; 512];
        assert_eq!(
            client.read_blocks(0, &mut buf),
            Err(DriverError::DeviceFault)
        );
    }
}
