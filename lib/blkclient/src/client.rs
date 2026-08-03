//! [`RemoteBlock`]: the [`Block`] client over the blkio request/reply pair,
//! and the [`BlkCall`] transport seam it is generic over.
//!
//! The transport call lives behind [`BlkCall`] so the client is
//! host-testable without a kernel: each call receives the client's mapping
//! of the shared window, which the production transport ([`crate::RtBlkCall`])
//! ignores (the serving driver fills the same frames through its own
//! mapping during the `ipc_call`) and a host test double fills directly,
//! playing the serving driver.

use tairix_abi::blkio::{
    decode_outcome, BlkCompletion, BlkDeviceClass, BlkOp, BlkOutcome, BlkRequest, IoBudget,
    BLK_COMPLETION_LEN, BLK_DATA_LEN, BLK_FLAG_READ_ONLY, BLK_REQUEST_LEN,
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
    /// reply length. `deadline_ns` bounds the wait: a device that has not
    /// answered within it fails this exchange closed rather than parking the
    /// caller forever. The caller derives it from the device's own
    /// [`IoBudget`], so the transport holds no deadline policy of its own.
    ///
    /// # Errors
    ///
    /// The transport's [`Errno`] (e.g. a vanished endpoint, or a device that
    /// consumed the whole deadline without answering).
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        window: &mut [u8],
        deadline_ns: u64,
    ) -> Result<usize, Errno>;
}

/// Which authority a [`RemoteBlock`] was opened with.
///
/// Not a `bool`: whether a caller may durably change a device is a
/// security-relevant stance, so the two [`RemoteBlock`] constructors name it
/// in a type a caller cannot get backwards by defaulting a boolean the wrong
/// way, or forget to check before wiring a new caller onto this client.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Access {
    /// [`Block::write_blocks`] always refuses and [`Block::flush`] is a
    /// truthful no-op, regardless of what the device reports. The
    /// volume-manager probe opens every device this way: it inspects a
    /// layout and commits nothing, so it is given no authority to change one.
    ReadOnly,
    /// [`Block::write_blocks`] and [`Block::flush`] reach the wire, subject
    /// to the device's own write-protect flag.
    ReadWrite,
}

/// A [`Block`] view of one served logical unit, opened under an explicit
/// access stance ([`RemoteBlock::connect_read_only`] /
/// [`RemoteBlock::connect_read_write`]).
pub struct RemoteBlock<'w, C: BlkCall> {
    call: C,
    /// This process's mapping of the shared data window the serving
    /// driver created (its length bounds every transfer chunk).
    window: &'w mut [u8],
    geometry: BlockGeometry,
    /// Whether the *device itself* reported [`BLK_FLAG_READ_ONLY`] — checked
    /// in addition to, never instead of, this client's own [`Access`].
    read_only: bool,
    /// The stance this client was opened under (see [`Access`]).
    access: Access,
    /// The shared per-device I/O budget, whose retry count bounds how many
    /// times a *reissuable* completion is reissued before failing closed —
    /// the same policy the kernel filesystem client obeys, so the two
    /// consumers cannot drift apart. It is derived from the class the device
    /// itself declares in its geometry completion, so a removable unit riding
    /// out a bus reset and a paravirtual device that has simply wedged are
    /// each given their own class's patience rather than one assumed envelope.
    budget: IoBudget,
    /// The class the device declared, kept so a composition layered over this
    /// client reports the real hardware's envelope rather than the
    /// unclassified default. `None` is a device that declared a class word
    /// this build does not recognise, held distinct from every named class so
    /// nothing above reports a medium the device never declared.
    declared_class: Option<BlkDeviceClass>,
}

impl<'w, C: BlkCall> RemoteBlock<'w, C> {
    /// Connect a **read/write** client: query and validate the device
    /// geometry, then allow [`Block::write_blocks`] and [`Block::flush`] to
    /// reach the wire (still refused when the device itself reports
    /// [`BLK_FLAG_READ_ONLY`]).
    ///
    /// # Errors
    ///
    /// See [`RemoteBlock::connect_read_only`].
    pub fn connect_read_write(call: C, window: &'w mut [u8]) -> Result<Self, Errno> {
        Self::connect_with_access(call, window, Access::ReadWrite)
    }

    /// Connect a **read-only** client: [`Block::write_blocks`] always
    /// refuses and [`Block::flush`] is a truthful no-op, regardless of what
    /// the device reports.
    ///
    /// This is the volume-manager probe's stance: it inspects a device's
    /// partition table and filesystem signatures and commits nothing, so
    /// opening it read-only keeps its authority as small as its job, rather
    /// than trusting every future caller to remember never to call
    /// `write_blocks`.
    ///
    /// # Errors
    ///
    /// * The transport's [`Errno`] when the geometry call fails.
    /// * [`Errno::OutOfRange`] for a hostile geometry (block size not a
    ///   power of two in 512..=4096, a zero block count, or a byte
    ///   capacity that overflows) or a window too small to hold one block.
    pub fn connect_read_only(call: C, window: &'w mut [u8]) -> Result<Self, Errno> {
        Self::connect_with_access(call, window, Access::ReadOnly)
    }

    /// The shared body of the two named constructors: query the device
    /// geometry and validate it before any consumer can issue a transfer.
    fn connect_with_access(call: C, window: &'w mut [u8], access: Access) -> Result<Self, Errno> {
        let mut client = Self {
            call,
            window,
            geometry: BlockGeometry {
                block_size: 0,
                block_count: 0,
            },
            read_only: false,
            access,
            // Until the device answers there is nothing to classify it by, so
            // the geometry query itself runs on the bounded unclassified
            // envelope; the device's own class is adopted below.
            declared_class: None,
            budget: BlkDeviceClass::served_as(None).budget(),
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
        client.declared_class = completion.class;
        client.budget = BlkDeviceClass::served_as(completion.class).budget();
        Ok(client)
    }

    /// Whether the device reported itself write-protected.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Issue one request, reissuing a *reissuable* completion up to the
    /// device's [`IoBudget::max_retries`] before failing closed.
    ///
    /// This is the consumer half of the reply-reissuable recovery model
    /// (`plans/FIX-IO.md` IO3), identical in policy to the kernel filesystem
    /// client: when the serving driver rides out a device blip it answers
    /// reissuably rather than with a hard fault, and this client reissues
    /// rather than failing an attempt for a device that is merely recovering.
    /// The reissue count is bounded, so a device that keeps answering
    /// reissuably still fails closed deterministically. Each reissue is a
    /// fresh request/reply exchange — event-driven, never a busy spin.
    fn transfer(&mut self, request: BlkRequest) -> Result<BlkCompletion, Errno> {
        let mut attempts: u32 = 0;
        loop {
            let outcome = self.transfer_once(request)?;
            if self.budget.should_reissue(outcome.status, attempts) {
                attempts += 1;
                continue;
            }
            return outcome.data();
        }
    }

    /// Issue exactly one request and decode its completion fail-closed. The
    /// reissue policy lives in [`transfer`](Self::transfer).
    fn transfer_once(&mut self, request: BlkRequest) -> Result<BlkOutcome, Errno> {
        let mut frame = [0u8; BLK_REQUEST_LEN];
        let len = request.encode(&mut frame)?;
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let got = self.call.call(
            &frame[..len],
            &mut reply,
            self.window,
            self.budget.deadline_ns,
        )?;
        Ok(decode_outcome(reply.get(..got).ok_or(Errno::BadMagic)?))
    }

    /// Largest whole-block chunk a single transfer can move through the
    /// window, in blocks (a `usize`: it is bounded by the window length).
    fn blocks_per_chunk(&self) -> usize {
        let window = self.window.len().min(BLK_DATA_LEN);
        window / self.geometry.block_size as usize
    }

    /// Validate a transfer's shape and extent against the cached geometry,
    /// before any request reaches the wire, returning the block count `len`
    /// covers. Shared by [`Block::read_blocks`] and [`Block::write_blocks`]
    /// so the two directions can never disagree on what a legal transfer
    /// looks like.
    fn check_extent(&self, lba: u64, len: usize) -> Result<usize, DriverError> {
        let block_size = self.geometry.block_size as usize;
        if len == 0 || !len.is_multiple_of(block_size) {
            return Err(DriverError::OutOfRange);
        }
        let blocks = len / block_size;
        // Chunk arithmetic stays in `usize` (both operands derive from
        // buffer lengths), so no width-truncating cast exists on any
        // pointer width.
        let end = lba
            .checked_add(blocks as u64)
            .ok_or(DriverError::OutOfRange)?;
        if end > self.geometry.block_count {
            return Err(DriverError::OutOfRange);
        }
        Ok(blocks)
    }
}

impl<C: BlkCall> Block for RemoteBlock<'_, C> {
    /// The class this client *serves* the device as, so a composition layered
    /// over it inherits the real hardware's envelope. A device whose declared
    /// class word this build does not recognise is served the bounded
    /// unclassified envelope, which buys it no extra patience.
    fn device_class(&self) -> BlkDeviceClass {
        BlkDeviceClass::served_as(self.declared_class)
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let block_size = self.geometry.block_size as usize;
        let blocks = self.check_extent(lba, buf.len())?;
        let mut done = 0usize;
        while done < blocks {
            let chunk_blocks = (blocks - done).min(self.blocks_per_chunk());
            let request = BlkRequest {
                op: BlkOp::Read,
                lba: lba + done as u64,
                blocks: u32::try_from(chunk_blocks).map_err(|_| DriverError::OutOfRange)?,
            };
            self.transfer(request).map_err(DriverError::from_errno)?;
            let chunk_bytes = chunk_blocks * block_size;
            let offset = done * block_size;
            buf[offset..offset + chunk_bytes].copy_from_slice(&self.window[..chunk_bytes]);
            done += chunk_blocks;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        // Both write-refusal reasons are checked before any byte moves: the
        // client's own opened stance, and the device's own declared
        // write-protect flag (defence in depth — a served device might
        // change its mind about neither, but this client trusts neither
        // alone).
        if self.access == Access::ReadOnly || self.read_only {
            return Err(DriverError::Unsupported);
        }
        let block_size = self.geometry.block_size as usize;
        let blocks = self.check_extent(lba, buf.len())?;
        let mut done = 0usize;
        while done < blocks {
            let chunk_blocks = (blocks - done).min(self.blocks_per_chunk());
            let chunk_bytes = chunk_blocks * block_size;
            let offset = done * block_size;
            self.window[..chunk_bytes].copy_from_slice(&buf[offset..offset + chunk_bytes]);
            let request = BlkRequest {
                op: BlkOp::Write,
                lba: lba + done as u64,
                blocks: u32::try_from(chunk_blocks).map_err(|_| DriverError::OutOfRange)?,
            };
            self.transfer(request).map_err(DriverError::from_errno)?;
            done += chunk_blocks;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        if self.access == Access::ReadOnly {
            // A client that never writes has nothing uncommitted; this is a
            // truthful no-op, not a swallowed forward.
            return Ok(());
        }
        self.transfer(BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        })
        .map_err(DriverError::from_errno)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
