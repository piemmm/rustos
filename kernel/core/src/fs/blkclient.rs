//! The kernel blkio client: a [`Block`] device over a user-space
//! block-service endpoint (`plans/DEVICES.md` D3b).
//!
//! A user-space block driver (the USB mass-storage class driver) serves
//! each logical unit as a call endpoint plus a shared data window
//! ([`rustos_abi::blkio`]). This client is the kernel-side consumer the
//! runtime volume attach path builds a filesystem on: it posts one bounded
//! [`BlkRequest`] at a time on the endpoint, wakes the serving driver,
//! parks the calling task until the reply lands (never a busy-poll — the
//! same post/park/wake discipline as the `ipc_call` handler), and moves
//! the data through its [`KernelHold`] on the shared window.
//!
//! The serving driver is untrusted: the geometry it reports is validated
//! fail-closed at [`BlkClient::connect`], every completion is decoded
//! fail-closed, and window bytes are treated as data (the filesystem layer
//! above validates all of it). A vanished endpoint (the device was
//! unplugged and the driver exited) surfaces as a typed device fault,
//! never a hang: the endpoint's destruction cancels the in-flight call and
//! wakes the parked task.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::blkio::{
    decode_completion, BlkCompletion, BlkOp, BlkRequest, BLK_COMPLETION_LEN, BLK_DATA_LEN,
    BLK_FLAG_READ_ONLY, BLK_REQUEST_LEN,
};
use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::DriverError;
use rustos_abi::{CapabilityId, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::{CallEndpoint, EndpointId, ReplyOutcome};
use rustos_kernel_sec::{TaskCapabilities, TaskId as SecTaskId, UserId};
use rustos_log::Sink;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;
use crate::sharedreg::KernelHold;
use crate::waitq::{serve_wake, serve_wake_task, wait_arch, CALL_WAITQ, NO_DEADLINE};

/// Reserved id space for the kernel blkio clients' claimant identities.
///
/// A claimant only matches its own tickets (the endpoint records the
/// poster per call), so this base merely keeps the synthetic ids readable
/// in audit records and disjoint from real task ids.
const CLIENT_ID_BASE: u64 = 0xB1D0_0000_0000_0000;

/// The next client claimant id to mint.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(CLIENT_ID_BASE);

/// Smallest logical block size the client accepts from a device.
const MIN_BLOCK_SIZE: u32 = 512;

/// Largest logical block size the client accepts from a device.
const MAX_BLOCK_SIZE: u32 = 4096;

/// A [`Block`] device served by a user-space block driver over a call
/// endpoint and a shared data window.
pub struct BlkClient {
    /// The per-LUN block-service endpoint.
    endpoint: Arc<CallEndpoint>,
    /// The kernel's counted hold on the shared data window.
    window: KernelHold,
    /// The synthetic kernel identity the client posts under; holds exactly
    /// `CAP_IPC_ENDPOINT` (the class capability a grant-restricted
    /// block-service endpoint requires of its senders) and nothing else.
    caps: TaskCapabilities,
    /// The audit sink post/reply decisions are recorded through.
    audit: &'static (dyn Sink + Sync),
    /// The device geometry, validated at connect time.
    geometry: BlockGeometry,
    /// Whether the device reported itself write-protected.
    read_only: bool,
}

/// Map a transport or service refusal onto the [`Block`] contract's error
/// space.
fn errno_to_driver(err: Errno) -> DriverError {
    match err {
        Errno::BufferTooSmall => DriverError::BufferTooSmall,
        Errno::LengthOutOfRange => DriverError::LengthOutOfRange,
        Errno::OutOfRange => DriverError::OutOfRange,
        Errno::PermissionDenied => DriverError::PermissionDenied,
        Errno::NoSpace => DriverError::NoSpace,
        // A vanished endpoint (`NotFound` — the driver exited on unplug),
        // a cancelled call, and every other transport failure is a device
        // fault: the device is gone or misbehaving, not the request.
        _ => DriverError::DeviceFault,
    }
}

impl BlkClient {
    /// Connect to the block service on `endpoint`, moving data through
    /// `window`, and validate the device geometry.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — the endpoint is not bound (the driver is
    ///   gone).
    /// * [`Errno::LengthOutOfRange`] — the window cannot hold one
    ///   [`BLK_DATA_LEN`] transfer.
    /// * [`Errno::MessageTooLarge`] — the endpoint's frame bounds cannot
    ///   carry the blkio protocol.
    /// * [`Errno::OutOfRange`] — the device reported an unusable geometry
    ///   (block size not a power of two in `512..=4096`, a zero block
    ///   count, or a window that cannot hold whole blocks).
    /// * Any transport error the geometry query surfaced.
    pub fn connect(
        endpoint: u64,
        window: KernelHold,
        audit: &'static (dyn Sink + Sync),
    ) -> Result<Self, Errno> {
        let endpoint = crate::callreg::lookup(EndpointId(endpoint)).ok_or(Errno::NotFound)?;
        if window.len() < BLK_DATA_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if BLK_REQUEST_LEN as u64 > u64::from(endpoint.max_request())
            || BLK_COMPLETION_LEN as u64 > u64::from(endpoint.max_reply())
        {
            return Err(Errno::MessageTooLarge);
        }
        // The client's synthetic identity: exactly the class capability a
        // grant-restricted block-service endpoint requires of its senders.
        // Minted only on the capability-checked attach path (`volume_attach`
        // requires `CAP_FS_MOUNT`), never ambient.
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::IPC_ENDPOINT);
        let caps = TaskCapabilities::derive(
            SecTaskId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)),
            UserId(0),
            caps,
            caps,
            audit,
        );
        let mut client = Self {
            endpoint,
            window,
            caps,
            audit,
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
        if !completion.block_size.is_power_of_two()
            || completion.block_size < MIN_BLOCK_SIZE
            || completion.block_size > MAX_BLOCK_SIZE
            || completion.block_count == 0
            || BLK_DATA_LEN % completion.block_size as usize != 0
        {
            return Err(Errno::OutOfRange);
        }
        client.geometry = BlockGeometry {
            block_size: completion.block_size,
            block_count: completion.block_count,
        };
        client.read_only = completion.flags & BLK_FLAG_READ_ONLY != 0;
        Ok(client)
    }

    /// Whether the device reported itself write-protected at connect time.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Commit every completed write to the medium (the blkio flush
    /// operation). Issued by the detach path before the volume's root is
    /// retracted, and available to a filesystem's own sync.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] (or a more specific refusal) when the
    /// device cannot commit — the caller fails closed rather than assuming
    /// durability.
    pub fn flush(&mut self) -> Result<(), DriverError> {
        self.transfer(BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        })
        .map(|_| ())
        .map_err(errno_to_driver)
    }

    /// Issue one request and block until its completion, mirroring the
    /// `ipc_call` handler's post → wake-server → park → take-reply
    /// discipline.
    fn transfer(&self, request: BlkRequest) -> Result<BlkCompletion, Errno> {
        let mut frame = [0u8; BLK_REQUEST_LEN];
        let len = request.encode(&mut frame)?;

        // The current scheduler identity, for park/wake. Absent a live
        // scheduler hook (a host test of this path, where the server runs
        // on another thread) there is nothing to park on, so the wait
        // degrades to a cooperative spin exactly as `SleepLock` does —
        // never reachable as a steady state on a running kernel.
        let sched = wait_arch().and_then(|hook| {
            hook.current_cpu()
                .and_then(|cpu| hook.current_task(cpu).map(|task| (cpu, task)))
        });

        // Post under the client's synthetic identity; the reply wakes the
        // *scheduler* task that is running this operation.
        let poster = sched.map_or(0, |(_, task)| task);
        let ticket = self
            .endpoint
            .post(&self.caps, poster, &frame[..len], self.audit)?;

        // Wake the serving driver parked between requests — exactly the
        // endpoint's recorded server where known, the broadcast fallback
        // otherwise (a thundering herd is worse than one spurious wake).
        match self.endpoint.server_task() {
            Some(server) => serve_wake_task(server),
            None => serve_wake(),
        }

        // Register before the first poll so a reply landing in the
        // check/park window is never lost, then poll-and-park until the
        // reply (or the endpoint's destruction) releases us.
        if let Some((_, task)) = sched {
            CALL_WAITQ.register(task, NO_DEADLINE);
        }
        let claimant = self.caps.task().0;
        let outcome = loop {
            match self.endpoint.take_reply(claimant, ticket) {
                ReplyOutcome::Ready(bytes) => break Ok(bytes),
                // The endpoint was torn down (the driver exited — the
                // device is gone) or the ticket is no longer ours: abandon
                // the call fail-closed.
                ReplyOutcome::Cancelled | ReplyOutcome::Unknown => break Err(Errno::NotFound),
                ReplyOutcome::Pending => match sched {
                    Some((cpu, _)) => {
                        if !reschedule_current(cpu, RescheduleAction::Park) {
                            core::hint::spin_loop();
                        }
                    }
                    None => core::hint::spin_loop(),
                },
            }
        };
        if let Some((_, task)) = sched {
            CALL_WAITQ.deregister(task);
        }
        decode_completion(&outcome?)
    }

    /// Copy `data` into the shared window (a write's payload).
    fn window_write(&self, data: &[u8]) {
        debug_assert!(data.len() <= self.window.len());
        // SAFETY: `connect` verified the window holds at least
        // `BLK_DATA_LEN` bytes and every caller bounds `data` by
        // `BLK_DATA_LEN`; the copy goes through raw pointers so no
        // reference over memory the serving driver may concurrently touch
        // is ever formed, and the source is a live kernel slice.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.window.as_ptr(), data.len());
        }
    }

    /// Copy a completed read's payload out of the shared window.
    fn window_read(&self, data: &mut [u8]) {
        debug_assert!(data.len() <= self.window.len());
        // SAFETY: as for `window_write` — bounded by the connect-time
        // window check, raw-pointer copy only, destination is a live
        // kernel slice. The bytes are device data the filesystem layer
        // validates; a torn concurrent write by a hostile driver yields
        // wrong *data*, never memory unsafety.
        unsafe {
            core::ptr::copy_nonoverlapping(self.window.as_ptr(), data.as_mut_ptr(), data.len());
        }
    }

    /// Validate a transfer's shape against the connect-time geometry,
    /// returning the block size. Local fail-fast; the serving driver
    /// re-checks everything against the live device.
    fn check_extent(&self, lba: u64, len: usize) -> Result<usize, DriverError> {
        let block_size = self.geometry.block_size as usize;
        if len == 0 || len % block_size != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (len / block_size) as u64;
        if lba
            .checked_add(blocks)
            .map_or(true, |end| end > self.geometry.block_count)
        {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(block_size)
    }
}

impl Block for BlkClient {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let block_size = self.check_extent(lba, buf.len())?;
        let mut lba = lba;
        let mut off = 0;
        while off < buf.len() {
            let chunk = (buf.len() - off).min(BLK_DATA_LEN);
            // Whole blocks by construction: `BLK_DATA_LEN` is a block-size
            // multiple (checked at connect) and so is `buf.len()`; the
            // count is at most `BLK_DATA_LEN / 512`, so the narrowing is
            // checked only defensively.
            let blocks =
                u32::try_from(chunk / block_size).map_err(|_| DriverError::LengthOutOfRange)?;
            self.transfer(BlkRequest {
                op: BlkOp::Read,
                lba,
                blocks,
            })
            .map_err(errno_to_driver)?;
            self.window_read(&mut buf[off..off + chunk]);
            off += chunk;
            lba += u64::from(blocks);
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        // The device's write policy, enforced before any byte moves (the
        // serving driver refuses too — defence in depth).
        if self.read_only {
            return Err(DriverError::Unsupported);
        }
        let block_size = self.check_extent(lba, buf.len())?;
        let mut lba = lba;
        let mut off = 0;
        while off < buf.len() {
            let chunk = (buf.len() - off).min(BLK_DATA_LEN);
            let blocks =
                u32::try_from(chunk / block_size).map_err(|_| DriverError::LengthOutOfRange)?;
            self.window_write(&buf[off..off + chunk]);
            self.transfer(BlkRequest {
                op: BlkOp::Write,
                lba,
                blocks,
            })
            .map_err(errno_to_driver)?;
            off += chunk;
            lba += u64::from(blocks);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use core::ptr::NonNull;
    use std::boxed::Box;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc as StdArc, Mutex};
    use std::thread;
    use std::vec;
    use std::vec::Vec;

    use rustos_abi::blkio::{encode_error_completion, BLK_FLAG_READ_ONLY};
    use rustos_kernel_ipc::{CallEndpointLimits, RecvCall};

    /// A throwaway audit sink.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }
    static SINK: NullSink = NullSink;

    const BLOCK_SIZE: usize = 512;
    /// `BLOCK_SIZE` as the wire-width type the geometry carries.
    const BLOCK_SIZE_U32: u32 = 512;

    /// Mint distinct endpoint ids per test so the global registry never
    /// clashes across the parallel test threads.
    fn fresh_endpoint_id() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(0xB1D0_7E57_0000_0000);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Create, register, and return an unrestricted block-service endpoint.
    fn register_endpoint(id: u64) -> Arc<CallEndpoint> {
        let creator = TaskCapabilities::derive(
            SecTaskId(0x7E57_0001),
            UserId(0),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &SINK,
        );
        let endpoint = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: u32::try_from(BLK_REQUEST_LEN).unwrap(),
                    max_reply: u32::try_from(BLK_COMPLETION_LEN).unwrap(),
                    capacity: 4,
                },
                &SINK,
            )
            .expect("endpoint"),
        );
        crate::callreg::register(Arc::clone(&endpoint), &SINK).expect("register");
        endpoint
    }

    /// The window one test leaks for the client and the server to share.
    /// Both sides touch it only through raw pointers, alternating by the
    /// request/reply protocol.
    struct Window(NonNull<u8>);
    // SAFETY: the window is leaked test memory alive for the process; the
    // request/reply protocol alternates access, and both sides copy through
    // raw pointers only.
    unsafe impl Send for Window {}

    impl Window {
        /// Accessor (rather than a field read) so a serving closure
        /// captures the whole `Send` wrapper, not the raw pointer field.
        fn ptr(&self) -> *mut u8 {
            self.0.as_ptr()
        }
    }

    fn leak_window() -> NonNull<u8> {
        NonNull::new(Box::leak(vec![0u8; BLK_DATA_LEN].into_boxed_slice()).as_mut_ptr())
            .expect("window")
    }

    /// An in-memory device the server thread serves: geometry, a byte
    /// store, a flush counter, and an optional read-only flag.
    struct MemDevice {
        data: Vec<u8>,
        flushes: usize,
        read_only: bool,
        reported_block_size: u32,
    }

    impl MemDevice {
        fn new(blocks: u64) -> Self {
            Self {
                data: vec![0u8; BLOCK_SIZE * usize::try_from(blocks).expect("fits")],
                flushes: 0,
                read_only: false,
                reported_block_size: BLOCK_SIZE_U32,
            }
        }

        fn block_count(&self) -> u64 {
            (self.data.len() / BLOCK_SIZE) as u64
        }
    }

    /// Serve `count` requests against `device` over `endpoint`, moving data
    /// through the shared `window`, then return the device.
    fn serve(
        endpoint: Arc<CallEndpoint>,
        window: Window,
        device: StdArc<Mutex<MemDevice>>,
        count: usize,
        stop: StdArc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut served = 0;
            while served < count && !stop.load(Ordering::Relaxed) {
                let call = match endpoint.recv_call(BLK_REQUEST_LEN) {
                    RecvCall::Received(call) => call,
                    RecvCall::Empty => {
                        thread::yield_now();
                        continue;
                    }
                    RecvCall::TooLarge { .. } => panic!("oversize request"),
                };
                let mut reply = [0u8; BLK_COMPLETION_LEN];
                let len = match BlkRequest::decode(&call.request) {
                    Err(err) => encode_error_completion(&mut reply, err).unwrap(),
                    Ok(request) => {
                        let mut device = device.lock().unwrap();
                        match request.op {
                            BlkOp::Geometry => BlkCompletion {
                                block_size: device.reported_block_size,
                                block_count: device.block_count(),
                                flags: if device.read_only {
                                    BLK_FLAG_READ_ONLY
                                } else {
                                    0
                                },
                            }
                            .encode(&mut reply)
                            .unwrap(),
                            BlkOp::Read => {
                                let start = usize::try_from(request.lba).unwrap() * BLOCK_SIZE;
                                let len = request.blocks as usize * BLOCK_SIZE;
                                if start + len > device.data.len() {
                                    encode_error_completion(&mut reply, Errno::LengthOutOfRange)
                                        .unwrap()
                                } else {
                                    // SAFETY: test window, protocol-alternated
                                    // access, length bounded by BLK_DATA_LEN.
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            device.data[start..].as_ptr(),
                                            window.ptr(),
                                            len,
                                        );
                                    }
                                    BlkCompletion::default().encode(&mut reply).unwrap()
                                }
                            }
                            BlkOp::Write => {
                                let start = usize::try_from(request.lba).unwrap() * BLOCK_SIZE;
                                let len = request.blocks as usize * BLOCK_SIZE;
                                if device.read_only {
                                    encode_error_completion(&mut reply, Errno::PermissionDenied)
                                        .unwrap()
                                } else if start + len > device.data.len() {
                                    encode_error_completion(&mut reply, Errno::LengthOutOfRange)
                                        .unwrap()
                                } else {
                                    // SAFETY: as for the read arm.
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            window.ptr(),
                                            device.data[start..].as_mut_ptr(),
                                            len,
                                        );
                                    }
                                    BlkCompletion::default().encode(&mut reply).unwrap()
                                }
                            }
                            BlkOp::Flush => {
                                device.flushes += 1;
                                BlkCompletion::default().encode(&mut reply).unwrap()
                            }
                        }
                    }
                };
                endpoint.reply(call.ticket, &reply[..len], &SINK).unwrap();
                served += 1;
            }
        })
    }

    /// Stand a device + server up and connect a client to it.
    fn connected(
        blocks: u64,
        requests: usize,
    ) -> (BlkClient, StdArc<Mutex<MemDevice>>, thread::JoinHandle<()>) {
        let id = fresh_endpoint_id();
        let endpoint = register_endpoint(id);
        let window = leak_window();
        let device = StdArc::new(Mutex::new(MemDevice::new(blocks)));
        let server = serve(
            endpoint,
            Window(window),
            StdArc::clone(&device),
            requests,
            StdArc::new(AtomicBool::new(false)),
        );
        let hold = KernelHold::for_test(window, BLK_DATA_LEN);
        let client = BlkClient::connect(id, hold, &SINK).expect("connect");
        (client, device, server)
    }

    #[test]
    fn connect_validates_geometry_and_write_policy() {
        // 1 geometry request.
        let (client, _device, server) = connected(64, 1);
        assert_eq!(
            client.geometry(),
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: 64,
            })
        );
        assert!(!client.read_only());
        server.join().unwrap();
        crate::callreg::unregister(EndpointId(0)); // no-op hygiene
    }

    #[test]
    fn write_then_read_round_trips_across_chunks() {
        // A transfer larger than one window splits into two requests:
        // geometry + 2 writes + 2 reads = 5.
        let blocks_len = BLK_DATA_LEN + BLOCK_SIZE;
        let (mut client, _device, server) = connected(256, 5);
        let mut out = vec![0u8; blocks_len];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::try_from(i % 251).unwrap();
        }
        client.write_blocks(3, &out).expect("write");
        let mut back = vec![0u8; blocks_len];
        client.read_blocks(3, &mut back).expect("read");
        assert_eq!(out, back);
        server.join().unwrap();
    }

    #[test]
    fn flush_reaches_the_device() {
        // geometry + flush = 2.
        let (mut client, device, server) = connected(8, 2);
        client.flush().expect("flush");
        server.join().unwrap();
        assert_eq!(device.lock().unwrap().flushes, 1);
    }

    #[test]
    fn a_read_only_device_refuses_writes_client_side() {
        let id = fresh_endpoint_id();
        let endpoint = register_endpoint(id);
        let window = leak_window();
        let device = StdArc::new(Mutex::new(MemDevice::new(8)));
        device.lock().unwrap().read_only = true;
        // Only the geometry request ever reaches the server.
        let server = serve(
            endpoint,
            Window(window),
            StdArc::clone(&device),
            1,
            StdArc::new(AtomicBool::new(false)),
        );
        let hold = KernelHold::for_test(window, BLK_DATA_LEN);
        let mut client = BlkClient::connect(id, hold, &SINK).expect("connect");
        assert!(client.read_only());
        assert_eq!(
            client.write_blocks(0, &[0u8; BLOCK_SIZE]),
            Err(DriverError::Unsupported)
        );
        server.join().unwrap();
    }

    #[test]
    fn extent_and_shape_violations_fail_before_any_request() {
        // geometry only.
        let (mut client, _device, server) = connected(8, 1);
        server.join().unwrap();
        let mut buf = [0u8; BLOCK_SIZE];
        // Past the end of the device.
        assert_eq!(
            client.read_blocks(8, &mut buf),
            Err(DriverError::LengthOutOfRange)
        );
        // An overflowing extent.
        assert_eq!(
            client.read_blocks(u64::MAX, &mut buf),
            Err(DriverError::LengthOutOfRange)
        );
        // Not a block multiple / empty.
        assert_eq!(
            client.read_blocks(0, &mut buf[..BLOCK_SIZE - 1]),
            Err(DriverError::BufferTooSmall)
        );
        assert_eq!(
            client.read_blocks(0, &mut buf[..0]),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn a_hostile_geometry_is_refused_at_connect() {
        for (block_size, blocks) in [(0u32, 8u64), (3, 8), (8192, 8), (512, 0)] {
            let id = fresh_endpoint_id();
            let endpoint = register_endpoint(id);
            let window = leak_window();
            let device = StdArc::new(Mutex::new(MemDevice::new(blocks.max(1))));
            {
                let mut device = device.lock().unwrap();
                device.reported_block_size = block_size;
                if blocks == 0 {
                    device.data.clear();
                }
            }
            let server = serve(
                endpoint,
                Window(window),
                StdArc::clone(&device),
                1,
                StdArc::new(AtomicBool::new(false)),
            );
            let hold = KernelHold::for_test(window, BLK_DATA_LEN);
            assert_eq!(
                BlkClient::connect(id, hold, &SINK).err(),
                Some(Errno::OutOfRange),
                "block_size={block_size} blocks={blocks}"
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn an_unknown_endpoint_or_short_window_fails_closed() {
        let hold = KernelHold::for_test(leak_window(), BLK_DATA_LEN);
        assert_eq!(
            BlkClient::connect(fresh_endpoint_id(), hold, &SINK).err(),
            Some(Errno::NotFound)
        );

        let id = fresh_endpoint_id();
        let _endpoint = register_endpoint(id);
        let short = KernelHold::for_test(leak_window(), BLK_DATA_LEN - 1);
        assert_eq!(
            BlkClient::connect(id, short, &SINK).err(),
            Some(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_destroyed_endpoint_surfaces_as_a_device_fault() {
        let (mut client, _device, server) = connected(8, 1);
        server.join().unwrap();
        // The driver exits (unplug): the endpoint is destroyed. The next
        // operation fails typed, never hangs.
        client.endpoint.destroy(&SINK);
        let mut buf = [0u8; BLOCK_SIZE];
        assert_eq!(
            client.read_blocks(0, &mut buf),
            Err(DriverError::DeviceFault)
        );
    }
}
