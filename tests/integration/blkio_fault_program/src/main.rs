//! FIX-IO IO2 fixture: the EL0 half of the block-transport fault vertical.
//!
//! The vertical's claim is that a wedged storage device can no longer lock up
//! the filesystem strata: the block seam is *time-bounded end to end*, a
//! wedged device fails closed at its own deadline, and an unrelated healthy
//! device keeps serving while that wedged request is outstanding. Host doubles
//! cannot express that — the per-request deadline, the reply wait-set, and the
//! ticket lifecycle are all kernel machinery — so this program exercises them
//! on the live kernel.
//!
//! It stands up two block-service endpoints of its own:
//!
//! * **the healthy device** — served through the *shared* per-request engine
//!   (`tairix_abi::blkio::serve_request_recovering`) over a fault-injecting
//!   `FaultDisk`, and consumed through the *production* client
//!   (`tairix_drv_storage_volmgr::blk::RemoteBlock`). Both halves are shipped
//!   code, so the vertical proves the real engine and the real consumer rather
//!   than a look-alike.
//! * **the wedged device** — an endpoint whose requests are deliberately never
//!   serviced, modelling a driver stalled on the medium.
//!
//! It then asserts, in order:
//!
//! 1. the healthy device connects, and reads return exactly the bytes the
//!    device holds;
//! 2. a transient device blip is *ridden out*: the serving engine answers
//!    reissuably while its grace window is open and the consumer reissues
//!    within the shared per-class budget, so the read returns correct data —
//!    and the device confirms it really did inject the faults;
//! 3. a blip that outlasts that budget still fails closed deterministically,
//!    as the typed transient class, never a generic device fault;
//! 4. a bad sector surfaces as the typed medium error, never collapsed into a
//!    whole-device fault;
//! 5. a request outstanding to the wedged device does **not** stop the healthy
//!    device: many healthy transfers complete while it is in flight, it never
//!    completes early, its reply wait-set member is woken by the elapsed
//!    deadline exactly like a real completion, and the claim then fails closed
//!    as a timeout no earlier than the deadline;
//! 6. a per-ticket cancel withdraws an outstanding request deterministically,
//!    and a foreign ticket cancels nothing (no existence oracle).
//!
//! Every failure returns its own distinct non-zero code, so the QEMU finisher
//! names the failing step.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::blkio::{
        serve_request_recovering, BlkDeviceClass, BlkHealth, BlkOp, BlkRequest, BLK_COMPLETION_LEN,
        BLK_REQUEST_LEN,
    };
    use tairix_abi::driver::block::{Block, BlockGeometry};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{DriverError, Errno};
    use tairix_caps::CapabilitySet;
    use tairix_drv_storage_volmgr::blk::{BlkCall, RemoteBlock};

    /// Endpoint id of the healthy block service this fixture stands up. The
    /// `b"BLKF"` tag keeps the pair clear of anything the chassis binds.
    const EP_HEALTHY: u64 = 0x424C_4B46_0000_0001;

    /// Endpoint id of the wedged block service: created and posted to, but
    /// never serviced, modelling a driver stalled on the medium.
    const EP_WEDGED: u64 = 0x424C_4B46_0000_0002;

    /// In-flight requests either endpoint admits at once. This fixture never
    /// holds more than two tickets; the headroom keeps a capacity refusal from
    /// masking a transport defect.
    const ENDPOINT_CAPACITY: usize = 4;

    /// The class the served device declares. Its budget is the single shared
    /// policy this fixture reads its reissue count from — never a literal of
    /// its own.
    const DEVICE_CLASS: BlkDeviceClass = BlkDeviceClass::Virtual;

    /// Logical block size the served device reports.
    const BLOCK_SIZE: u32 = 512;

    /// [`BLOCK_SIZE`] as a buffer length.
    const BLOCK_LEN: usize = 512;

    /// Logical block count the served device reports.
    const BLOCK_COUNT: u64 = 64;

    /// The shared data window a transfer moves through. Client and server are
    /// the same address space here, so one buffer *is* the shared window.
    const WINDOW_LEN: usize = 4096;

    /// A block that always reads back its content pattern.
    const GOOD_LBA: u64 = 1;

    /// A block whose first reads fault transiently, then succeed — the blip
    /// the recovery grace window exists to ride out.
    const BLIP_LBA: u64 = 2;

    /// A block that faults transiently on *every* read — the device that keeps
    /// answering reissuably and must still fail closed at the budget.
    const FOREVER_BLIP_LBA: u64 = 3;

    /// A block that always reports a permanent medium error.
    const BAD_LBA: u64 = 4;

    /// A block whose content reports how many transient faults the device has
    /// injected so far, so the ride-out cannot pass vacuously.
    const COUNTER_LBA: u64 = 5;

    /// Transient faults injected at [`BLIP_LBA`]: exactly the number the
    /// shared per-class policy permits reissuing, so a successful read proves
    /// the whole budget is honoured rather than some arbitrary retry count.
    const BLIP_FAULTS: u32 = DEVICE_CLASS.budget().max_retries;

    /// Healthy transfers issued while the wedged request is outstanding. Under
    /// an unbounded transport not one of them could have completed, so the
    /// count is the head-of-line-freedom assertion.
    const INTERLEAVED_READS: u32 = 16;

    /// Deadline the wedged request is posted with. Short enough that the
    /// vertical costs a fraction of a second, long enough that the interleaved
    /// healthy transfers comfortably precede it.
    const WEDGE_DEADLINE_NS: u64 = 300_000_000;

    /// Ceiling on the park that waits for the wedged deadline to wake us.
    /// More than thirty times the deadline, so it is reachable only if the
    /// deadline never woke the waiter at all — a mechanism failure, never a
    /// slow host — and still comfortably inside the chassis watchdog.
    const WEDGE_WAKE_BUDGET_NS: u64 = 10_000_000_000;

    /// Wait-set token the wedged endpoint's reply member carries.
    const WEDGE_TOKEN: u64 = 0xF0F0;

    /// A ticket this fixture never minted, used to prove a foreign ticket
    /// cancels nothing.
    const FOREIGN_TICKET: u64 = 0xDEAD_BEEF;

    /// The deterministic byte a served block holds at `offset`.
    fn pattern(lba: u64, offset: usize) -> u8 {
        // Both narrowings are the point: the pattern is one byte wide.
        #[allow(clippy::cast_possible_truncation)]
        let base = lba as u8;
        #[allow(clippy::cast_possible_truncation)]
        let index = offset as u8;
        base.wrapping_mul(31).wrapping_add(index)
    }

    /// Recover an [`Errno`] from a raw negative kernel result (`-errno`).
    fn errno_from(neg: i64) -> Errno {
        Errno::from_i32(i32::try_from(-neg).unwrap_or(0)).unwrap_or(Errno::NotFound)
    }

    /// Claim `ticket`'s reply without blocking, with the raw `-errno` decoded.
    fn reap(endpoint: u64, ticket: u64, reply: &mut [u8]) -> Result<usize, Errno> {
        tairix_rt::call_reap(endpoint, ticket, reply).map_err(errno_from)
    }

    /// The fixture's served device: a deterministic pattern disk with three
    /// scripted fault sites, so every health class the transport must keep
    /// distinct is produced by the *device*, never fabricated in a completion
    /// frame.
    struct FaultDisk {
        /// Transient faults still owed at [`BLIP_LBA`].
        blip_owed: u32,
        /// Transient faults injected so far, readable at [`COUNTER_LBA`].
        injected: u32,
    }

    impl FaultDisk {
        /// A fresh device owing its scripted blip.
        const fn new() -> Self {
            Self {
                blip_owed: BLIP_FAULTS,
                injected: 0,
            }
        }

        /// Fill one block's worth of `buf` for `lba`, or report the fault the
        /// script owes that block.
        fn fill_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            match lba {
                BAD_LBA => return Err(DriverError::MediumError),
                FOREVER_BLIP_LBA => {
                    self.injected = self.injected.saturating_add(1);
                    return Err(DriverError::Busy);
                }
                BLIP_LBA if self.blip_owed > 0 => {
                    self.blip_owed -= 1;
                    self.injected = self.injected.saturating_add(1);
                    return Err(DriverError::Busy);
                }
                _ => {}
            }
            for (offset, byte) in buf.iter_mut().enumerate() {
                *byte = pattern(lba, offset);
            }
            if lba == COUNTER_LBA {
                buf[..4].copy_from_slice(&self.injected.to_le_bytes());
            }
            Ok(())
        }
    }

    impl Block for FaultDisk {
        fn device_class(&self) -> BlkDeviceClass {
            DEVICE_CLASS
        }

        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            if buf.is_empty() || !buf.len().is_multiple_of(BLOCK_LEN) {
                return Err(DriverError::OutOfRange);
            }
            let blocks =
                u64::try_from(buf.len() / BLOCK_LEN).map_err(|_| DriverError::OutOfRange)?;
            let end = lba.checked_add(blocks).ok_or(DriverError::OutOfRange)?;
            if end > BLOCK_COUNT {
                return Err(DriverError::OutOfRange);
            }
            // The length was checked to be a whole number of blocks above, so
            // the trailing remainder is empty.
            let (whole_blocks, _) = buf.as_chunks_mut::<BLOCK_LEN>();
            for (block, chunk) in (lba..).zip(whole_blocks) {
                self.fill_block(block, chunk)?;
            }
            Ok(())
        }

        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            // The fixture never writes; refusing keeps the device's authority
            // as small as its job.
            Err(DriverError::Unsupported)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// The healthy device's transport: a real round trip over the endpoint's
    /// own ticketed call machinery, with this process playing the serving
    /// driver in between.
    ///
    /// This is the block seam's documented test double — the seam exists so a
    /// consumer can be driven without a separate driver process — but unlike a
    /// host double it goes through the live kernel: the request is
    /// `call_post`ed with the caller's per-request deadline, taken off the
    /// endpoint with a non-blocking `call_recv`, answered by the shared
    /// per-request recovery engine, replied, and reaped. The codec, the ticket
    /// lifecycle, the engine, and the device health machine are therefore all
    /// the shipped ones.
    struct ServedLoopback {
        /// The endpoint both halves of the exchange use.
        endpoint: u64,
        /// The reply wait-set, created on first use and `0` until then.
        waitset: u64,
        /// The served device.
        device: FaultDisk,
        /// The device's health machine, driven by the monotonic clock.
        health: BlkHealth,
    }

    impl ServedLoopback {
        /// A transport serving a fresh device on `endpoint`.
        const fn new(endpoint: u64) -> Self {
            Self {
                endpoint,
                waitset: 0,
                device: FaultDisk::new(),
                health: BlkHealth::new(DEVICE_CLASS),
            }
        }

        /// Mint the reply wait-set and register this endpoint's reply member,
        /// once.
        fn ensure_waitset(&mut self) -> Result<u64, Errno> {
            if self.waitset == 0 {
                self.waitset = create_reply_waitset(self.endpoint, 0)?;
            }
            Ok(self.waitset)
        }

        /// Play the serving driver for one queued request: take it off the
        /// endpoint and answer it through the shared engine.
        fn serve_one(&mut self, window: &mut [u8]) -> Result<(), Errno> {
            let mut request = [0u8; BLK_REQUEST_LEN];
            let mut ticket = 0u64;
            let len = tairix_rt::call_recv_nonblock(self.endpoint, &mut request, &mut ticket)
                .map_err(errno_from)?;
            let mut frame = [0u8; BLK_COMPLETION_LEN];
            let reply_len = serve_request_recovering(
                &mut self.device,
                false,
                &request[..len],
                window,
                &mut frame,
                &mut self.health,
                tairix_rt::clock_get(),
            );
            let ret = tairix_rt::call_reply(self.endpoint, ticket, &frame[..reply_len]);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }
    }

    impl BlkCall for ServedLoopback {
        fn call(
            &mut self,
            request: &[u8],
            reply: &mut [u8],
            window: &mut [u8],
            deadline_ns: u64,
        ) -> Result<usize, Errno> {
            let set = self.ensure_waitset()?;
            let ticket =
                tairix_rt::call_post(self.endpoint, request, deadline_ns).map_err(errno_from)?;
            self.serve_one(window)?;
            loop {
                match reap(self.endpoint, ticket, reply) {
                    Ok(len) => return Ok(len),
                    // Not ready yet: park on the reply wait-set until the
                    // reply lands or the deadline elapses, never a busy poll.
                    // Every other outcome fails closed.
                    Err(Errno::WouldBlock) => {
                        let mut token = 0u64;
                        let _ = tairix_rt::waitset_wait(set, deadline_ns, &mut token);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }

    /// Bind one block-service endpoint this process both posts to and serves.
    ///
    /// Both capability sets are empty: the endpoint carries no send or receive
    /// requirement of its own, so binding it needs no privileged-bind
    /// authority and the kernel's own checks are the only gate.
    fn create_endpoint(id: u64) -> bool {
        let none = CapabilitySet::empty();
        tairix_rt::call_create(
            id,
            &none,
            &none,
            BLK_REQUEST_LEN,
            BLK_COMPLETION_LEN,
            ENDPOINT_CAPACITY,
        ) == 0
    }

    /// Create a wait-set observing `endpoint`'s reply completions under
    /// `token`.
    fn create_reply_waitset(endpoint: u64, token: u64) -> Result<u64, Errno> {
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return Err(errno_from(set));
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the minted handle.
        let set = set as u64;
        let ctl = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::CallReply,
            endpoint,
            token,
        );
        if ctl < 0 {
            return Err(errno_from(ctl));
        }
        Ok(set)
    }

    /// Post one geometry request to the wedged endpoint with `deadline_ns`.
    fn post_wedged(deadline_ns: u64) -> Result<u64, Errno> {
        let mut frame = [0u8; BLK_REQUEST_LEN];
        let len = BlkRequest {
            op: BlkOp::Geometry,
            lba: 0,
            blocks: 0,
        }
        .encode(&mut frame)?;
        tairix_rt::call_post(EP_WEDGED, &frame[..len], deadline_ns).map_err(errno_from)
    }

    /// Read one block and check it holds exactly the pattern the device stores
    /// for `lba`.
    fn read_and_verify<C: BlkCall>(
        client: &mut RemoteBlock<'_, C>,
        lba: u64,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        client.read_blocks(lba, buf)?;
        for (offset, byte) in buf.iter().enumerate() {
            if *byte != pattern(lba, offset) {
                return Err(DriverError::DeviceFault);
            }
        }
        Ok(())
    }

    /// Prove a transient device blip is ridden out, that a blip outlasting the
    /// shared budget still fails closed as the typed transient class, and that
    /// a bad sector keeps its own typed class.
    fn check_recovery<C: BlkCall>(client: &mut RemoteBlock<'_, C>, buf: &mut [u8]) -> i32 {
        // The blip resolves inside the budget, so the workload sees correct
        // data rather than a spurious I/O error.
        if read_and_verify(client, BLIP_LBA, buf).is_err() {
            return 30;
        }
        // ... and the device confirms it really injected the faults, so the
        // ride-out above cannot have passed vacuously.
        if client.read_blocks(COUNTER_LBA, buf).is_err() {
            return 31;
        }
        let injected = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if injected != BLIP_FAULTS {
            return 32;
        }
        // A device that keeps answering reissuably still fails closed
        // deterministically at the budget, as the typed transient class.
        if client.read_blocks(FOREVER_BLIP_LBA, buf) != Err(DriverError::Busy) {
            return 33;
        }
        // A bad sector is a permanent, per-request fault: it keeps its own
        // typed class and is never collapsed into a whole-device fault.
        if client.read_blocks(BAD_LBA, buf) != Err(DriverError::MediumError) {
            return 34;
        }
        0
    }

    /// Prove a request outstanding to a wedged device neither blocks an
    /// unrelated healthy device nor completes early, that its elapsed deadline
    /// wakes the parked reaper exactly like a real completion, and that the
    /// claim then fails closed as a timeout.
    fn check_wedged_isolation<C: BlkCall>(client: &mut RemoteBlock<'_, C>, buf: &mut [u8]) -> i32 {
        let Ok(set) = create_reply_waitset(EP_WEDGED, WEDGE_TOKEN) else {
            return 40;
        };
        let Ok(ticket) = post_wedged(WEDGE_DEADLINE_NS) else {
            return 41;
        };
        let posted_ns = tairix_rt::clock_get();

        let mut reply = [0u8; BLK_COMPLETION_LEN];
        // Nothing has served it and its deadline has not elapsed, so the
        // non-blocking claim must say so rather than invent an outcome.
        if reap(EP_WEDGED, ticket, &mut reply) != Err(Errno::WouldBlock) {
            return 42;
        }

        // The healthy device keeps serving while the wedged request is in
        // flight.
        for _ in 0..INTERLEAVED_READS {
            if read_and_verify(client, GOOD_LBA, buf).is_err() {
                return 43;
            }
            // The wedged request must not complete early either: while its
            // deadline is still ahead of us the claim stays pending.
            if tairix_rt::clock_get().saturating_sub(posted_ns) < WEDGE_DEADLINE_NS
                && reap(EP_WEDGED, ticket, &mut reply) != Err(Errno::WouldBlock)
            {
                return 44;
            }
        }

        // Park until the wedged deadline makes the reply member ready.
        let mut token = 0u64;
        if tairix_rt::waitset_wait(set, WEDGE_WAKE_BUDGET_NS, &mut token) < 0 {
            return 45;
        }
        if token != WEDGE_TOKEN {
            return 46;
        }
        if tairix_rt::clock_get().saturating_sub(posted_ns) < WEDGE_DEADLINE_NS {
            return 47;
        }
        // The claim now fails closed as a timeout — never a fabricated
        // success, and never a different error class.
        if reap(EP_WEDGED, ticket, &mut reply) != Err(Errno::TimedOut) {
            return 48;
        }
        0
    }

    /// Prove a per-ticket cancel withdraws an outstanding request
    /// deterministically, and that a foreign ticket cancels nothing.
    fn check_cancel() -> i32 {
        let Ok(ticket) = post_wedged(WEDGE_WAKE_BUDGET_NS) else {
            return 50;
        };
        if tairix_rt::call_cancel(EP_WEDGED, FOREIGN_TICKET) >= 0 {
            return 51;
        }
        if tairix_rt::call_cancel(EP_WEDGED, ticket) != 0 {
            return 52;
        }
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        if reap(EP_WEDGED, ticket, &mut reply) != Err(Errno::NotFound) {
            return 53;
        }
        0
    }

    /// Program entry point: stand the two devices up and run the checks in
    /// order, returning the first failing step's own code.
    fn main() -> i32 {
        if !create_endpoint(EP_HEALTHY) {
            return 10;
        }
        if !create_endpoint(EP_WEDGED) {
            return 11;
        }

        let mut window = [0u8; WINDOW_LEN];
        let Ok(mut client) = RemoteBlock::connect(ServedLoopback::new(EP_HEALTHY), &mut window)
        else {
            return 12;
        };
        if client.read_only() {
            return 13;
        }

        let mut buf = [0u8; BLOCK_LEN];
        if read_and_verify(&mut client, GOOD_LBA, &mut buf).is_err() {
            return 20;
        }

        let recovery = check_recovery(&mut client, &mut buf);
        if recovery != 0 {
            return recovery;
        }
        let isolation = check_wedged_isolation(&mut client, &mut buf);
        if isolation != 0 {
            return isolation;
        }
        check_cancel()
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
