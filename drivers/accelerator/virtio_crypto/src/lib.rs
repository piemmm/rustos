//! TAIRiX virtio-crypto accelerator driver.
//!
//! Implements [`tairix_abi::driver::accelerator::Accelerator`] over the
//! bus-agnostic virtio protocol from `lib/virtio`, so the same source drives
//! the device on either transport: a virtio-crypto device is the same protocol
//! whether it arrives on PCI or on an MMIO slot.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point. [`VirtioCrypto`] is a public
//! *type* the hosting binary and the host tests instantiate; nothing reaches
//! into it beyond the `Accelerator` trait.
//!
//! # A session per job, and no key kept
//!
//! virtio-crypto binds a key *and a direction* into a device-side session
//! (virtio 1.2 §5.9.7.1.1), which a throughput-minded driver would create once
//! and reuse. This one creates the session, runs the job and destroys the
//! session inside the single [`Accelerator::cipher`] call, because reuse would
//! mean holding the caller's key to compare the next job's against — and a
//! retained key is a key a compromised driver can be made to use again. The
//! cost is two extra control-queue round trips per job; the guarantee bought
//! is that no key material outlives the call that supplied it, on either side
//! of the device boundary. A consumer that needs the throughput needs a
//! caller-owned session handle on the class trait, which is a different
//! interface and arrives with that consumer.
//!
//! # The device is untrusted
//!
//! Every figure read from the device's configuration space is treated as
//! hostile input: the advertised per-request ceiling is clamped to
//! [`MAX_STAGED_JOB_BYTES`] before it sizes any allocation, an unrecognised
//! algorithm bit is ignored rather than offered, and a device that reports
//! itself not ready, offers no cipher service, or advertises no data queue is
//! refused at bring-up instead of driven.
//!
//! # Status decoding
//!
//! Both the control and the data path decode their reply through one shared
//! `status_to_result`, so neither can classify an outcome the other would read
//! differently. The mapping keeps an honest axis: a rejected key or an
//! unsupported request is a request-level refusal the caller can act on, and
//! any *undefined* status byte fails closed to
//! [`DriverError::DeviceFault`] rather than being assumed benign.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// Only the unit tests allocate; the driver's job path is `alloc`-free by
// design (persistent DMA staging carved once at open, no per-job heap copy).
#[cfg(test)]
extern crate alloc;

use tairix_abi::driver::accelerator::{
    Accelerator, AcceleratorDeviceReport, CipherAlgorithm, CipherAlgorithms, CipherDirection,
    CipherJob,
};
use tairix_abi::driver::{BufferClass, CompletionSignal};
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_virtio::{
    BounceBuffer, ChainSegment, Direction, DmaSlab, SplitQueue, Status, Transport, VirtioError,
    VirtioHost,
};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x5643_5259_0000_0001; // "VCRY"

/// The virtio device id of a crypto device (virtio 1.2 §5.9 — `virtio-crypto`
/// is device type 20). [`BIND_KEYS`] is built from it, so a discovered virtio
/// node whose probed device id is 20 binds this driver and nothing else.
pub const VIRTIO_CRYPTO_DEVICE_ID: u32 = 20;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is *exact* — the discovered node's probed device
/// id either is `virtio-crypto` or it is not, there being no wildcard — so it
/// ranks at the same exact-match tier as the other concrete-identity virtio
/// drivers.
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a virtio crypto device, matched by its
/// virtio device id ([`VIRTIO_CRYPTO_DEVICE_ID`]).
///
/// The single source of truth the signed-manifest bind table is authored from
/// and the device manager resolves a discovered node against. The key carries
/// no transport detail, so the one bundle binds the device however it is
/// attached.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_CRYPTO_DEVICE_ID),
)];

/// Most input bytes one job may stage, and so the size of each of the two
/// persistent payload buffers carved at open.
///
/// A containment bound, not a capacity, and it bounds two things at once. The
/// device advertises its own per-request ceiling in configuration space, and
/// that figure is *the device's* — so a hostile or broken device could
/// otherwise make the driver demand an arbitrarily large DMA allocation at
/// bring-up; the staging is therefore the smaller of the two. And a cipher
/// needs a device-read source *and* a device-write destination, so the
/// driver's DMA footprint is twice this figure however small each job is:
/// 32 KiB total, the same order as the block driver's single 32 KiB staging
/// window, which is what keeps an accelerator affordable on a machine with
/// several devices and little memory rather than the heaviest DMA consumer in
/// the tree.
///
/// A job above the published ceiling is refused rather than split, because
/// splitting a cipher-block-chaining job means carrying the previous block's
/// cipher text into the next job's initialisation vector — the *caller's*
/// chaining decision to make, not a transformation the driver may apply
/// silently. The ceiling reaches the caller as
/// [`AcceleratorDeviceReport::max_job_bytes`], so it is a figure to plan
/// against rather than a surprise.
pub const MAX_STAGED_JOB_BYTES: u64 = 16 * 1024;

/// Most key bytes one session may stage: the longest key any algorithm in
/// [`CipherAlgorithm`] accepts (AES-256).
const MAX_KEY_BYTES: usize = 32;

/// Longest initialisation vector any algorithm in [`CipherAlgorithm`] uses.
const MAX_IV_BYTES: usize = 16;

/// Nanoseconds one submitted chain may go unanswered before the job fails
/// closed.
///
/// A fault-isolation bound: without it a caller parks inside the job for ever
/// if the device's completion interrupt is lost, coalesced or never raised,
/// which is a hang rather than a fault and invisible to the caller. Two
/// seconds is far longer than any job this staging can hold takes on a device
/// that is answering at all — a job of the whole
/// [`MAX_STAGED_JOB_BYTES`] ceiling is microseconds of work — so it can only
/// ever trip on a device that has stopped.
const JOB_DEADLINE_NS: u64 = 2_000_000_000;

/// Upper bound on advisory completion wakes a single chain tolerates before
/// failing closed.
///
/// Jobs are serialised by the owner, so exactly one completion is ever
/// outstanding, and a healthy device posts it within a wake or two of the
/// notify. A count far above that turns a pathological stream of wakes with no
/// matching completion — a stuck or mis-routed shared interrupt — into a
/// deterministic fault rather than an unbounded loop, without ever tripping in
/// normal operation.
const MAX_COMPLETION_WAKES: u32 = 1024;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// Virtio-crypto wire protocol constants and layout (virtio 1.2 §5.9).
mod wire {
    /// `VIRTIO_F_VERSION_1` (bit 32): the modern virtio 1.x split-virtqueue
    /// layout, required of a non-transitional device.
    pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

    /// Configuration-space offsets (`struct virtio_crypto_config`).
    pub mod config {
        /// `status`, whose bit 0 is `VIRTIO_CRYPTO_S_HW_READY`.
        pub const STATUS: usize = 0;
        /// `max_dataqueues`: how many data queues precede the control queue.
        pub const MAX_DATAQUEUES: usize = 4;
        /// `crypto_services`: the service families the device offers.
        pub const CRYPTO_SERVICES: usize = 8;
        /// `cipher_algo_l`: cipher algorithms 0..32.
        pub const CIPHER_ALGO_L: usize = 12;
        /// `max_size`: the most content one request may carry.
        pub const MAX_SIZE: usize = 48;
    }

    /// `VIRTIO_CRYPTO_S_HW_READY`: the accelerator hardware is ready.
    pub const S_HW_READY: u32 = 1 << 0;
    /// `VIRTIO_CRYPTO_SERVICE_CIPHER` bit in `crypto_services`.
    pub const SERVICE_CIPHER: u32 = 1 << 0;
    /// `VIRTIO_CRYPTO_CIPHER_AES_CBC` bit in `cipher_algo_l`.
    pub const CIPHER_AES_CBC_BIT: u32 = 1 << 3;
    /// `VIRTIO_CRYPTO_CIPHER_AES_CBC` as an `algo` field value.
    pub const CIPHER_AES_CBC: u32 = 3;

    /// `VIRTIO_CRYPTO_CIPHER_CREATE_SESSION` = `OPCODE(SERVICE_CIPHER, 0x02)`.
    pub const CIPHER_CREATE_SESSION: u32 = 0x0002;
    /// `VIRTIO_CRYPTO_CIPHER_DESTROY_SESSION` = `OPCODE(SERVICE_CIPHER, 0x03)`.
    pub const CIPHER_DESTROY_SESSION: u32 = 0x0003;
    /// `VIRTIO_CRYPTO_CIPHER_ENCRYPT` = `OPCODE(SERVICE_CIPHER, 0x00)`.
    pub const CIPHER_ENCRYPT: u32 = 0x0000;
    /// `VIRTIO_CRYPTO_CIPHER_DECRYPT` = `OPCODE(SERVICE_CIPHER, 0x01)`.
    pub const CIPHER_DECRYPT: u32 = 0x0001;

    /// `VIRTIO_CRYPTO_OP_ENCRYPT`, the session's bound direction.
    pub const OP_ENCRYPT: u32 = 1;
    /// `VIRTIO_CRYPTO_OP_DECRYPT`, the session's bound direction.
    pub const OP_DECRYPT: u32 = 2;
    /// `VIRTIO_CRYPTO_SYM_OP_CIPHER`: a plain cipher, not an algorithm chain.
    pub const SYM_OP_CIPHER: u32 = 1;

    /// `struct virtio_crypto_op_ctrl_req`: a 16-byte header plus a 56-byte
    /// operation-specific union. Also the length of
    /// `struct virtio_crypto_op_data_req` (a 24-byte header plus a 48-byte
    /// union), so one staging buffer serves both — a control and a data chain
    /// are never in flight together.
    pub const REQ_LEN: usize = 72;
    /// Offset of `sym_create_session.cipher.para.algo` within a control
    /// request: past the 16-byte control header.
    pub const CTRL_CIPHER_PARA: usize = 16;
    /// Offset of `sym_create_session.op_type`: past the header and the
    /// 48-byte inner union.
    pub const CTRL_SYM_OP_TYPE: usize = 64;
    /// Offset of `destroy_session.session_id`: past the control header.
    pub const CTRL_DESTROY_SESSION_ID: usize = 16;
    /// Offset of `sym_req.cipher.para.iv_len` within a data request: past the
    /// 24-byte operation header.
    pub const DATA_CIPHER_PARA: usize = 24;
    /// Offset of `sym_req.op_type`: past the header and the 40-byte inner
    /// union.
    pub const DATA_SYM_OP_TYPE: usize = 64;

    /// `struct virtio_crypto_session_input`: the device-written reply to a
    /// session create — `session_id` (8), `status` (4), padding (4).
    pub const SESSION_INPUT_LEN: usize = 16;
    /// `struct virtio_crypto_inhdr`: the one-byte device-written status a
    /// session destroy and a data request both reply with.
    pub const INHDR_LEN: usize = 1;

    /// `VIRTIO_CRYPTO_OK`. The status vocabulary is one byte wide because
    /// that is what a data request's reply carries; a session create carries
    /// the same values in a 32-bit field, which the decode narrows.
    pub const STATUS_OK: u8 = 0;
    /// `VIRTIO_CRYPTO_ERR`: the device failed the request.
    pub const STATUS_ERR: u8 = 1;
    /// `VIRTIO_CRYPTO_BADMSG`: the device could not parse the request.
    pub const STATUS_BADMSG: u8 = 2;
    /// `VIRTIO_CRYPTO_NOTSUPP`: the device does not implement the request.
    pub const STATUS_NOTSUPP: u8 = 3;
    /// `VIRTIO_CRYPTO_INVSESS`: the named session does not exist.
    pub const STATUS_INVSESS: u8 = 4;
    /// `VIRTIO_CRYPTO_NOSPC`: no free session id.
    pub const STATUS_NOSPC: u8 = 5;
    /// `VIRTIO_CRYPTO_KEY_REJECTED`: the device refused the key.
    pub const STATUS_KEY_REJECTED: u8 = 6;
}

/// Map a virtio-crypto status word to a driver outcome.
///
/// The one decode both the control and the data path use, keeping an honest
/// axis: an unsupported request, a rejected key and an exhausted session table
/// are request-level refusals a caller can act on, while a device error, an
/// unparseable request, a session the device has lost, and any status this ABI
/// does not define are device faults. Failing an undefined value closed is
/// deliberate — a status nobody defines must never read as success.
///
/// # Errors
///
/// One of the mapped [`DriverError`]s; never `Ok` for a non-zero status.
fn status_to_result(status: u8) -> Result<(), DriverError> {
    match status {
        wire::STATUS_OK => Ok(()),
        wire::STATUS_NOTSUPP => Err(DriverError::Unsupported),
        wire::STATUS_KEY_REJECTED => Err(DriverError::PermissionDenied),
        wire::STATUS_NOSPC => Err(DriverError::Busy),
        wire::STATUS_ERR | wire::STATUS_BADMSG | wire::STATUS_INVSESS => {
            Err(DriverError::DeviceFault)
        }
        _ => Err(DriverError::DeviceFault),
    }
}

/// A virtio-crypto accelerator behind a cross-arch virtio transport.
///
/// `'h` bounds the borrow of the [`VirtioHost`] the driver allocates its DMA
/// regions through: the host is minted per driver load and lives only for the
/// duration of that load, so the driver borrows it rather than demanding a
/// `'static` one.
pub struct VirtioCrypto<'h, T: Transport> {
    transport: T,
    /// The data queue jobs are submitted on (index 0 — the driver uses one of
    /// however many the device offers, because jobs are serialised).
    dataq: SplitQueue,
    /// The control queue sessions are created and destroyed on (index
    /// `max_dataqueues`, wherever the device put it).
    controlq: SplitQueue,
    host: &'h dyn VirtioHost,
    /// The device's own report, read once at bring-up: its memory, the
    /// algorithms it offered that this driver recognises, and the clamped
    /// per-job ceiling.
    report: AcceleratorDeviceReport,
    /// Persistent staging carved once at open and reused by every job, so the
    /// job path never re-enters the DMA allocator. `None` only while a job
    /// holds them in class-aware [`BounceBuffer`] wrappers.
    req: Option<DmaSlab>,
    key: Option<DmaSlab>,
    iv: Option<DmaSlab>,
    src: Option<DmaSlab>,
    dst: Option<DmaSlab>,
    session: Option<DmaSlab>,
    status: Option<DmaSlab>,
}

impl<'h, T: Transport> VirtioCrypto<'h, T> {
    /// Bring the device online.
    ///
    /// Runs the virtio 1.2 §3.1 initialisation sequence — reset,
    /// `ACKNOWLEDGE`, `DRIVER`, negotiate `VIRTIO_F_VERSION_1` and no
    /// device-specific feature, `FEATURES_OK`, set up the data and control
    /// queues, `DRIVER_OK` — then reads the device's configuration space and
    /// refuses a device this driver cannot honestly drive.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device reports itself not ready,
    ///   offers no cipher service, or offers no cipher algorithm this driver
    ///   implements. A driver that bound such a device would accept jobs it
    ///   could only fail.
    /// * [`DriverError::DeviceFault`] if the device advertises no data queue,
    ///   rejects the negotiated features, or a queue or staging allocation
    ///   fails.
    pub fn open(mut transport: T, host: &'h dyn VirtioHost) -> Result<Self, DriverError> {
        transport.reset();
        let mut status = Status::default().with(Status::ACKNOWLEDGE);
        transport.set_status(status);
        status = status.with(Status::DRIVER);
        transport.set_status(status);
        // No device-specific feature is negotiated: the stateless-mode bits
        // change the request shape, and this driver implements the session
        // shape, so accepting one would promise behaviour it does not honour.
        let driver_features = transport.device_features() & wire::VIRTIO_F_VERSION_1;
        transport.set_driver_features(driver_features);
        status = status.with(Status::FEATURES_OK);
        transport.set_status(status);
        if !transport.status().contains(Status::FEATURES_OK) {
            return Err(VirtioError::FeaturesRejected.as_driver_error());
        }

        // Every figure below is the device's, so it is validated before it
        // sizes anything or decides anything.
        if read_config_u32(&transport, wire::config::STATUS) & wire::S_HW_READY == 0 {
            return Err(DriverError::Unsupported);
        }
        if read_config_u32(&transport, wire::config::CRYPTO_SERVICES) & wire::SERVICE_CIPHER == 0 {
            return Err(DriverError::Unsupported);
        }
        let ciphers = offered_ciphers(read_config_u32(&transport, wire::config::CIPHER_ALGO_L));
        if ciphers.is_empty() {
            return Err(DriverError::Unsupported);
        }
        // The control queue sits immediately after the data queues, so its
        // index is the device's own `max_dataqueues`. A device offering none
        // has nowhere to submit a job.
        let data_queues = read_config_u32(&transport, wire::config::MAX_DATAQUEUES);
        if data_queues == 0 {
            return Err(DriverError::DeviceFault);
        }
        let control_index = u16::try_from(data_queues).map_err(|_| DriverError::DeviceFault)?;

        let dataq = open_queue(&mut transport, host, DATA_QUEUE)?;
        let controlq = open_queue(&mut transport, host, control_index)?;
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);

        // The staged ceiling is the smaller of what the device will carry and
        // what this driver will allocate for it; a device declaring no
        // ceiling of its own gets the driver's.
        let advertised = read_config_u64(&transport, wire::config::MAX_SIZE);
        let ceiling = if advertised == 0 {
            MAX_STAGED_JOB_BYTES
        } else {
            advertised.min(MAX_STAGED_JOB_BYTES)
        };
        let staged = usize::try_from(ceiling).map_err(|_| DriverError::LengthOutOfRange)?;

        let carve = |len: usize| host.alloc_dma_zeroed(len);
        Ok(Self {
            transport,
            dataq,
            controlq,
            host,
            report: AcceleratorDeviceReport {
                // A virtio-crypto device declares no memory of its own: it
                // works out of the driver's DMA staging, which is system RAM.
                // Reporting a figure here would be inventing one.
                mem_resident_bytes: 0,
                mem_total_bytes: 0,
                ciphers,
                max_job_bytes: ceiling,
            },
            req: Some(carve(wire::REQ_LEN)?),
            key: Some(carve(MAX_KEY_BYTES)?),
            iv: Some(carve(MAX_IV_BYTES)?),
            src: Some(carve(staged)?),
            dst: Some(carve(staged)?),
            session: Some(carve(wire::SESSION_INPUT_LEN)?),
            status: Some(carve(wire::INHDR_LEN)?),
        })
    }

    /// Tear the device down for unload (sets the status byte to 0).
    pub fn close(mut self) {
        self.transport.reset();
    }

    /// Borrow the underlying transport (host-side test access only; not
    /// exposed across the driver-class trait surface).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Borrow the underlying transport mutably for the in-process software
    /// peer to drive on `kick`.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Create a device-side session for `job`'s key and direction, returning
    /// its id.
    ///
    /// The key is staged into a [`BufferClass::Sensitive`] bounce buffer, so
    /// the staging is scrubbed when the buffer is put back regardless of how
    /// the session round trip ended.
    fn create_session(&mut self, job: &CipherJob<'_>) -> Result<u64, DriverError> {
        let (Some(req), Some(key), Some(session)) =
            (self.req.take(), self.key.take(), self.session.take())
        else {
            return Err(DriverError::DeviceFault);
        };
        let mut req_bb = BounceBuffer::new(req, BufferClass::NonSensitive);
        let mut key_bb = BounceBuffer::new(key, BufferClass::Sensitive);
        let mut session_bb = BounceBuffer::new(session, BufferClass::NonSensitive);
        let result = self.exchange_create_session(&mut req_bb, &mut key_bb, &mut session_bb, job);
        self.req = Some(req_bb.into_slab());
        self.key = Some(key_bb.into_slab());
        self.session = Some(session_bb.into_slab());
        result
    }

    /// Stage the session-create request, publish the chain, wait, and decode
    /// the device's reply. Split out of [`Self::create_session`] so every
    /// early return still lets the caller put the staging back.
    fn exchange_create_session(
        &mut self,
        req_bb: &mut BounceBuffer,
        key_bb: &mut BounceBuffer,
        session_bb: &mut BounceBuffer,
        job: &CipherJob<'_>,
    ) -> Result<u64, DriverError> {
        let mut frame = [0u8; wire::REQ_LEN];
        put_u32(&mut frame, 0, wire::CIPHER_CREATE_SESSION);
        // The session's own parameter block carries the algorithm; the
        // header's `algo` is unused for a session create. `queue_id` names
        // the data queue the session will be used on.
        put_u32(&mut frame, 12, u32::from(DATA_QUEUE));
        put_u32(
            &mut frame,
            wire::CTRL_CIPHER_PARA,
            algo_value(job.algorithm),
        );
        let key_len = u32::try_from(job.key.len()).map_err(|_| DriverError::OutOfRange)?;
        put_u32(&mut frame, wire::CTRL_CIPHER_PARA + 4, key_len);
        put_u32(
            &mut frame,
            wire::CTRL_CIPHER_PARA + 8,
            session_op(job.direction),
        );
        put_u32(&mut frame, wire::CTRL_SYM_OP_TYPE, wire::SYM_OP_CIPHER);
        req_bb.stage(&frame)?;
        key_bb.stage(job.key)?;

        let segments = [
            segment(req_bb.phys(), wire::REQ_LEN, Direction::DeviceRead)?,
            segment(key_bb.phys(), job.key.len(), Direction::DeviceRead)?,
            segment(
                session_bb.phys(),
                wire::SESSION_INPUT_LEN,
                Direction::DeviceWrite,
            )?,
        ];
        submit_and_wait(
            &mut self.controlq,
            &mut self.transport,
            self.host,
            &segments,
        )?;
        let reply = session_bb.full_region_mut();
        let id = read_u64(reply, 0);
        let status = u8::try_from(read_u32(reply, 8)).map_err(|_| DriverError::DeviceFault)?;
        status_to_result(status)?;
        Ok(id)
    }

    /// Destroy the device-side session `id`.
    ///
    /// Called on the way out of every [`Accelerator::cipher`], including one
    /// whose job failed: a session left behind holds the device's copy of the
    /// caller's key schedule and consumes one of the device's session slots.
    fn destroy_session(&mut self, id: u64) -> Result<(), DriverError> {
        let (Some(req), Some(status)) = (self.req.take(), self.status.take()) else {
            return Err(DriverError::DeviceFault);
        };
        let mut req_bb = BounceBuffer::new(req, BufferClass::NonSensitive);
        let mut status_bb = BounceBuffer::new(status, BufferClass::NonSensitive);
        let result = self.exchange_destroy_session(&mut req_bb, &mut status_bb, id);
        self.req = Some(req_bb.into_slab());
        self.status = Some(status_bb.into_slab());
        result
    }

    /// Stage the session-destroy request, publish the chain, wait, and decode
    /// the one-byte reply.
    fn exchange_destroy_session(
        &mut self,
        req_bb: &mut BounceBuffer,
        status_bb: &mut BounceBuffer,
        id: u64,
    ) -> Result<(), DriverError> {
        let mut frame = [0u8; wire::REQ_LEN];
        put_u32(&mut frame, 0, wire::CIPHER_DESTROY_SESSION);
        put_u32(&mut frame, 12, u32::from(DATA_QUEUE));
        put_u64(&mut frame, wire::CTRL_DESTROY_SESSION_ID, id);
        req_bb.stage(&frame)?;
        let segments = [
            segment(req_bb.phys(), wire::REQ_LEN, Direction::DeviceRead)?,
            segment(status_bb.phys(), wire::INHDR_LEN, Direction::DeviceWrite)?,
        ];
        submit_and_wait(
            &mut self.controlq,
            &mut self.transport,
            self.host,
            &segments,
        )?;
        status_to_result(status_bb.full_region_mut()[0])
    }

    /// Run `job` against session `id` on the data queue, copying the device's
    /// output back to the caller only once the device reported success.
    fn run_job(&mut self, id: u64, job: &mut CipherJob<'_>) -> Result<(), DriverError> {
        let (Some(req), Some(iv), Some(src), Some(dst), Some(status)) = (
            self.req.take(),
            self.iv.take(),
            self.src.take(),
            self.dst.take(),
            self.status.take(),
        ) else {
            return Err(DriverError::DeviceFault);
        };
        let mut req_bb = BounceBuffer::new(req, BufferClass::NonSensitive);
        let mut iv_bb = BounceBuffer::new(iv, BufferClass::NonSensitive);
        // Both payload buffers hold caller plain or cipher text, so both are
        // scrubbed when they are put back.
        let mut src_bb = BounceBuffer::new(src, BufferClass::Sensitive);
        let mut dst_bb = BounceBuffer::new(dst, BufferClass::Sensitive);
        let mut status_bb = BounceBuffer::new(status, BufferClass::NonSensitive);
        let result = self.exchange_job(
            &mut req_bb,
            &mut iv_bb,
            &mut src_bb,
            &mut dst_bb,
            &mut status_bb,
            id,
            job,
        );
        self.req = Some(req_bb.into_slab());
        self.iv = Some(iv_bb.into_slab());
        self.src = Some(src_bb.into_slab());
        self.dst = Some(dst_bb.into_slab());
        self.status = Some(status_bb.into_slab());
        result
    }

    /// Stage the data request, publish the chain, wait, decode the status,
    /// and only then hand the device's output to the caller.
    #[allow(clippy::too_many_arguments)] // Each buffer is a distinct
                                         // descriptor of the one published chain; grouping them into a struct
                                         // would name the chain twice without making either half clearer.
    fn exchange_job(
        &mut self,
        req_bb: &mut BounceBuffer,
        iv_bb: &mut BounceBuffer,
        src_bb: &mut BounceBuffer,
        dst_bb: &mut BounceBuffer,
        status_bb: &mut BounceBuffer,
        id: u64,
        job: &mut CipherJob<'_>,
    ) -> Result<(), DriverError> {
        let len = u32::try_from(job.input.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let iv_len = u32::try_from(job.iv.len()).map_err(|_| DriverError::OutOfRange)?;
        let mut frame = [0u8; wire::REQ_LEN];
        put_u32(&mut frame, 0, data_opcode(job.direction));
        put_u32(&mut frame, 4, algo_value(job.algorithm));
        put_u64(&mut frame, 8, id);
        put_u32(&mut frame, wire::DATA_CIPHER_PARA, iv_len);
        put_u32(&mut frame, wire::DATA_CIPHER_PARA + 4, len);
        put_u32(&mut frame, wire::DATA_CIPHER_PARA + 8, len);
        put_u32(&mut frame, wire::DATA_SYM_OP_TYPE, wire::SYM_OP_CIPHER);
        req_bb.stage(&frame)?;
        iv_bb.stage(job.iv)?;
        src_bb.stage(job.input)?;

        let segments = [
            segment(req_bb.phys(), wire::REQ_LEN, Direction::DeviceRead)?,
            segment(iv_bb.phys(), job.iv.len(), Direction::DeviceRead)?,
            segment(src_bb.phys(), job.input.len(), Direction::DeviceRead)?,
            segment(dst_bb.phys(), job.output.len(), Direction::DeviceWrite)?,
            segment(status_bb.phys(), wire::INHDR_LEN, Direction::DeviceWrite)?,
        ];
        submit_and_wait(&mut self.dataq, &mut self.transport, self.host, &segments)?;
        // Gated on the status: the destination staging persists across jobs,
        // so copying before this check could hand the caller bytes an earlier
        // job left behind.
        status_to_result(status_bb.full_region_mut()[0])?;
        let produced = job.output.len();
        job.output
            .copy_from_slice(&dst_bb.full_region_mut()[..produced]);
        Ok(())
    }
}

impl<T: Transport> Accelerator for VirtioCrypto<'_, T> {
    fn device_report(&self) -> AcceleratorDeviceReport {
        self.report
    }

    fn cipher(&mut self, mut job: CipherJob<'_>) -> Result<(), DriverError> {
        job.validate(self.report.ciphers)?;
        let len = u64::try_from(job.input.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        if len > self.report.max_job_bytes {
            return Err(DriverError::LengthOutOfRange);
        }
        let id = self.create_session(&job)?;
        let outcome = self.run_job(id, &mut job);
        // The session is destroyed either way: a failed job must not leave
        // the device holding the caller's key schedule. A destroy that itself
        // fails is reported only when the job succeeded, so a job's own
        // refusal is not masked by the cleanup behind it.
        let teardown = self.destroy_session(id);
        outcome.and(teardown)
    }
}

/// The data queue index this driver submits jobs on.
///
/// A device may offer several, but jobs are serialised by the owner, so one is
/// what the driver uses; the others exist for a multi-queue consumer that does
/// not yet exist.
const DATA_QUEUE: u16 = 0;

/// Set up `queue`, taking the deepest ring the device offers up to the depth
/// one serialised in-flight chain needs.
///
/// The chains this driver publishes are at most five descriptors, and exactly
/// one is ever outstanding, so a deeper ring would be memory the driver can
/// never use.
fn open_queue<T: Transport>(
    transport: &mut T,
    host: &dyn VirtioHost,
    index: u16,
) -> Result<SplitQueue, DriverError> {
    transport
        .queue_select(index)
        .map_err(VirtioError::as_driver_error)?;
    let size = transport.queue_max_size().min(QUEUE_SIZE);
    if size == 0 {
        return Err(DriverError::DeviceFault);
    }
    SplitQueue::new(transport, host, index, size).map_err(VirtioError::as_driver_error)
}

/// Ring depth each queue is programmed with: enough for the longest chain
/// this driver publishes, and no more.
const QUEUE_SIZE: u16 = 8;

/// Publish `segments` on `queue`, kick the device, and wait for the single
/// outstanding completion, acknowledging the device interrupt before
/// returning.
///
/// A wake is only *advisory*: the device's used-`idx` write and its interrupt
/// can be observed in either order, and one shared line can wake the driver
/// for another queue, so a single empty scan is never proof that no completion
/// is coming. The ring is re-scanned after every wake and waited on again only
/// when it is genuinely empty. Two independent bounds keep an unwell device
/// from stalling the caller, because neither catches the other's failure
/// shape: [`JOB_DEADLINE_NS`] releases the caller from *silence*, and
/// [`MAX_COMPLETION_WAKES`] from *noise* — a wake storm with no matching
/// completion, which no deadline would catch because each wake resets the
/// wait. Both fail the job closed with a typed error.
fn submit_and_wait<T: Transport>(
    queue: &mut SplitQueue,
    transport: &mut T,
    host: &dyn VirtioHost,
    segments: &[ChainSegment],
) -> Result<(), DriverError> {
    queue
        .add_chain(segments)
        .map_err(VirtioError::as_driver_error)?;
    queue.kick(transport);
    let mut outcome: Result<(), DriverError> = Err(DriverError::DeviceFault);
    let mut silent = false;
    for _ in 0..MAX_COMPLETION_WAKES {
        match queue.poll_used() {
            Ok(_token) => {
                outcome = Ok(());
                break;
            }
            Err(VirtioError::NoCompletion) => {
                if silent {
                    // The deadline elapsed and this final re-scan still finds
                    // nothing: the device is present but not answering. Fail
                    // closed rather than reissuing — the device may still own
                    // the published chain, so re-publishing the same staging
                    // could have it write an abandoned request's output into
                    // the next job's buffers.
                    outcome = Err(DriverError::DeviceOffline);
                    break;
                }
                if host.notify_wait(queue.index(), JOB_DEADLINE_NS) == CompletionSignal::TimedOut {
                    // Re-scan once before giving up: a completion whose
                    // interrupt was lost or coalesced is already in the ring,
                    // and a wait timing out says nothing about its contents.
                    silent = true;
                }
            }
            Err(e) => {
                outcome = Err(e.as_driver_error());
                break;
            }
        }
    }
    // Acknowledge the device's interrupt now its completion has been observed
    // (or the wait gave up), so it de-asserts its line before the next chain
    // re-arms the kernel IRQ — otherwise a stale edge re-delivers and the
    // following chain mis-pairs its completion. A no-op on transports that
    // need no device-side acknowledge.
    transport.ack_interrupt();
    outcome
}

/// One chain descriptor over a staged buffer, refusing a length no
/// descriptor can name rather than truncating it.
fn segment(phys: u64, len: usize, direction: Direction) -> Result<ChainSegment, DriverError> {
    Ok(ChainSegment {
        phys,
        len: u32::try_from(len).map_err(|_| DriverError::LengthOutOfRange)?,
        direction,
    })
}

/// The [`CipherAlgorithm`]s in a device's `cipher_algo_l` word that this
/// driver implements.
///
/// Bits this driver does not recognise are ignored rather than offered: a
/// device advertising an algorithm the driver cannot encode a request for
/// would otherwise have jobs accepted and then failed by the device.
fn offered_ciphers(algo_l: u32) -> CipherAlgorithms {
    let mut offered = CipherAlgorithms::NONE;
    if algo_l & wire::CIPHER_AES_CBC_BIT != 0 {
        offered = offered.with(CipherAlgorithm::AesCbc);
    }
    offered
}

/// The device's `algo` field value for `algorithm`.
const fn algo_value(algorithm: CipherAlgorithm) -> u32 {
    match algorithm {
        CipherAlgorithm::AesCbc => wire::CIPHER_AES_CBC,
    }
}

/// The `op` a session is created with, which binds its direction.
const fn session_op(direction: CipherDirection) -> u32 {
    match direction {
        CipherDirection::Encrypt => wire::OP_ENCRYPT,
        CipherDirection::Decrypt => wire::OP_DECRYPT,
    }
}

/// The data-request opcode for `direction`.
const fn data_opcode(direction: CipherDirection) -> u32 {
    match direction {
        CipherDirection::Encrypt => wire::CIPHER_ENCRYPT,
        CipherDirection::Decrypt => wire::CIPHER_DECRYPT,
    }
}

/// Read one little-endian `u32` from the device-configuration window.
fn read_config_u32<T: Transport>(transport: &T, offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    transport.read_config(offset, &mut buf);
    u32::from_le_bytes(buf)
}

/// Read one little-endian `u64` from the device-configuration window.
fn read_config_u64<T: Transport>(transport: &T, offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    transport.read_config(offset, &mut buf);
    u64::from_le_bytes(buf)
}

/// Write `value` little-endian at `offset`. The frames are fixed-size arrays
/// whose offsets are compile-time constants of this module, so a slot that
/// did not fit would be a coding error in the layout above rather than a
/// runtime condition — the write is simply skipped instead of panicking.
fn put_u32(frame: &mut [u8], offset: usize, value: u32) {
    if let Some(slot) = frame.get_mut(offset..offset + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// Write `value` little-endian at `offset`, as [`put_u32`].
fn put_u64(frame: &mut [u8], offset: usize, value: u64) {
    if let Some(slot) = frame.get_mut(offset..offset + 8) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// Read a little-endian `u32` from a device-written reply, reading zero for a
/// reply too short to hold one — a shape the descriptor lengths make
/// impossible, and which must not be a panic if the device somehow produces
/// it.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|slot| <[u8; 4]>::try_from(slot).ok())
        .map_or(0, u32::from_le_bytes)
}

/// Read a little-endian `u64` from a device-written reply, as [`read_u32`].
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    bytes
        .get(offset..offset + 8)
        .and_then(|slot| <[u8; 8]>::try_from(slot).ok())
        .map_or(0, u64::from_le_bytes)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
