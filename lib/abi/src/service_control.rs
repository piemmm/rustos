//! The service-manager control IPC protocol (`plans/NEW-SERVICEMANAGER.md`
//! SVC-8).
//!
//! The service manager (PID 1, and a per-user manager instance) owns four
//! reserved synchronous call endpoints:
//!
//! * [`SERVICE_CONTROL_ENDPOINT`] drives a registered service's **runtime**
//!   lifecycle — `start` a down service now, or `stop` a running one;
//! * [`SERVICE_ENROL_ENDPOINT`] changes its **persistent** enrolment —
//!   `enable` or `disable`, which the manager records in the registration
//!   store and obeys on the next boot;
//! * [`SERVICE_ACTIVATION_ENDPOINT`] brokers a **client's** connection to a
//!   shared service, activating it on demand and parking the client until it
//!   is ready;
//! * [`SERVICE_NOTICE_ENDPOINT`] carries a service's own self-report
//!   ([`crate::ServiceNotice`]) — a readiness announcement or a liveness
//!   renewal — attributed to the kernel-attested sender.
//!
//! They are four endpoints rather than operations on one because the acts
//! differ in authority and the answers differ in kind: an administrator
//! reaches the first two, any client the third, and a service speaks only for
//! itself on the fourth. Keeping them apart is what lets their gates diverge
//! without reshaping any of the protocols, and what makes them scope-derived
//! together when a per-user manager is spawned. Observability (`status`) is on
//! none of them: it is served through the System Information API, never a
//! control-reply scrape.
//!
//! This module is the wire contract for the three name-carrying requests
//! (the notice's own frame is [`crate::ServiceNotice`], which carries no name),
//! modelled on the
//! read-only mailbox and font protocols ([`crate::mailbox_ipc`],
//! [`crate::font_ipc`]): a fixed-size, bounds-checked request framing and a
//! status-framed reply, both little-endian, `no_std` and allocation-free (the
//! request name borrows the caller's buffer). Like those, it is an IPC-protocol
//! module, so it is outside the generated C-ABI header and its decoders are
//! enrolled in the `lib/abi` fuzz harness.
//!
//! **Authorization is the endpoint's, not this module's.** The kernel gates
//! *reaching* the endpoint on the capability the manager binds it with (the
//! reserved-bind gate plus a required send capability), so the receiver does
//! not re-check a caller capability here. The request carries only the
//! operation and the target service name; the manager validates the name
//! against its own strict service-name policy before it touches any state (fail
//! closed).
//!
//! Both requests share one fixed [`REQUEST_LEN`]-byte frame (little-endian),
//! so there is a single framing to audit and fuzz; only the magic and the
//! operation vocabulary differ:
//!
//! ```text
//!  0  magic u32     = SERVICE_CONTROL_MAGIC | SERVICE_ENROL_MAGIC
//!  4  version u16   = SERVICE_CONTROL_VERSION_V1
//!  6  op u16        (ServiceControlOp | ServiceEnrolOp)
//!  8  name_len u16  (0..=SERVICE_MANIFEST_MAX_NAME_LEN)
//! 10  reserved u16  = 0
//! 12  name [SERVICE_MANIFEST_MAX_NAME_LEN] (name_len bytes, rest zero)
//! ```
//!
//! A frame carrying one endpoint's magic can therefore never be accepted by
//! the other's decoder, so an operation cannot be smuggled onto the endpoint
//! whose authority it was not meant for.

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::service::SERVICE_MANIFEST_MAX_NAME_LEN;
use crate::{Duration64, Errno, ServiceEnrolment, ServiceState};

/// Well-known kernel-owned call-endpoint id of the service manager's control
/// surface (`"SVC\0"` little-endian).
///
/// The manager creates one synchronous call endpoint under this reserved id
/// with [`SyscallNumber::CALL_CREATE`](crate::SyscallNumber::CALL_CREATE); a
/// control tool names it as the `endpoint` argument to
/// [`SyscallNumber::IPC_CALL`](crate::SyscallNumber::IPC_CALL). A reserved
/// well-known id keeps the tool/manager rendezvous from needing a prior
/// name-exchange step; the endpoint's required send capability still gates
/// every call ([`crate::ipc::is_reserved_endpoint`]).
pub const SERVICE_CONTROL_ENDPOINT: u64 = 0x5356_4300;

/// Magic number identifying an `abi-v1` service-control request
/// (`"SVCC"` little-endian).
pub const SERVICE_CONTROL_MAGIC: u32 = u32::from_le_bytes(*b"SVCC");

/// Well-known kernel-owned call-endpoint id of the service manager's
/// **enrolment** surface (`"SVE\0"` little-endian).
///
/// The sibling of [`SERVICE_CONTROL_ENDPOINT`], bound by the same manager and
/// reserved the same way. It is a second id rather than a second operation on
/// the first because a persistent enrolment change and a runtime start/stop are
/// different acts with different answers; keeping the endpoints apart is what
/// lets their gates diverge later without reshaping either protocol.
pub const SERVICE_ENROL_ENDPOINT: u64 = 0x5356_4500;

/// Magic number identifying an `abi-v1` service-enrolment request
/// (`"SVCE"` little-endian).
///
/// Distinct from [`SERVICE_CONTROL_MAGIC`], so a frame built for one endpoint
/// is refused by the other's decoder before its operation is even classified.
pub const SERVICE_ENROL_MAGIC: u32 = u32::from_le_bytes(*b"SVCE");

/// Well-known kernel-owned call-endpoint id of the service manager's
/// **activation** surface (`"SVA\0"` little-endian).
///
/// The third sibling of [`SERVICE_CONTROL_ENDPOINT`] /
/// [`SERVICE_ENROL_ENDPOINT`], and a separate id for the same reason they
/// are separate from each other: brokering a *client's* connection to a
/// shared service is a different act from an administrator starting one, so
/// their gates are different and stay free to diverge. A client reaches this
/// endpoint; only an administrator reaches the other two.
pub const SERVICE_ACTIVATION_ENDPOINT: u64 = 0x5356_4100;

/// Magic number identifying an `abi-v1` service-activation request
/// (`"SVCA"` little-endian).
pub const SERVICE_ACTIVATION_MAGIC: u32 = u32::from_le_bytes(*b"SVCA");

/// Well-known kernel-owned call-endpoint id of the service manager's
/// **lifecycle-notice** surface (`"SVN\0"` little-endian).
///
/// The fourth sibling of the three endpoints above, separate for the same
/// reason they are separate from each other: a service reporting on its
/// *own* progress is a different act from an administrator driving a service
/// or a client brokering a connection, and it is gated differently. The other
/// three answer a request about *some* service the caller names; this one
/// only ever resolves to the caller's own, because the manager attributes the
/// notice to the kernel-attested sender and the frame
/// ([`crate::ServiceNotice`]) carries no service name at all.
///
/// A readiness announcement and a liveness renewal share the endpoint
/// because they are the same act — a service reporting on itself — and the
/// manager resolves both from the same attested sender. They differ only in
/// which state the sender must be in for the report to mean anything, which
/// is the manager's to decide, not a second rendezvous to bind.
///
/// The endpoint therefore needs no send capability: reaching it lets a
/// principal say one thing about itself, and a principal the manager cannot
/// match to a service in the state that report requires is refused with
/// [`Errno::NotFound`] before any state is touched.
pub const SERVICE_NOTICE_ENDPOINT: u64 = 0x5356_4E00;

/// Version of the service-control protocol carried in every request.
pub const SERVICE_CONTROL_VERSION_V1: u16 = 1;

// Request field offsets.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_OP: usize = 6;
const OFF_NAME_LEN: usize = 8;
const OFF_RESERVED: usize = 10;
const OFF_NAME: usize = 12;

/// Encoded length of a service-control request (its fixed prefix plus the
/// bounded name field). This is also the endpoint's maximum request size.
pub const REQUEST_LEN: usize = OFF_NAME + SERVICE_MANIFEST_MAX_NAME_LEN;

/// Fixed prefix of every reply frame: a status word (`0` on success, else the
/// negated [`Errno`] discriminant), mirroring [`crate::mailbox_ipc`].
const REPLY_STATUS_LEN: usize = 4;

/// Encoded length of a service-control reply: the status word, the resulting
/// [`ServiceState`] byte, and a reserved tail. This is also the endpoint's
/// maximum reply size.
pub const REPLY_LEN: usize = REPLY_STATUS_LEN + 4;

/// Encode a request frame: the shared prefix, `op`, and the bounded `name`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] for a short buffer, [`Errno::LengthOutOfRange`]
/// for an over-bound name.
fn encode_frame(buf: &mut [u8], magic: u32, op: u16, name: &str) -> Result<usize, Errno> {
    if buf.len() < REQUEST_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let name = name.as_bytes();
    if name.len() > SERVICE_MANIFEST_MAX_NAME_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    // Zero the whole frame first so the unused name tail and reserved field
    // are canonical (all-zero), matching the decoder's checks.
    buf[..REQUEST_LEN].fill(0);
    put_u32(buf, OFF_MAGIC, magic);
    put_u16(buf, OFF_VERSION, SERVICE_CONTROL_VERSION_V1);
    put_u16(buf, OFF_OP, op);
    // `name.len() <= SERVICE_MANIFEST_MAX_NAME_LEN` fits u16 by construction.
    #[allow(clippy::cast_possible_truncation)]
    put_u16(buf, OFF_NAME_LEN, name.len() as u16);
    // OFF_RESERVED stays zero from the wipe above.
    buf[OFF_NAME..OFF_NAME + name.len()].copy_from_slice(name);
    Ok(REQUEST_LEN)
}

/// Decode a request frame, validating the whole of it up front, and return the
/// raw operation discriminant and the borrowed name.
///
/// The caller classifies the discriminant against its own operation set, so a
/// frame addressed to the sibling endpoint is refused by its `magic` before any
/// operation is considered.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] for a short frame or an over-bound `name_len`,
/// [`Errno::AbiVersionUnsupported`] for a wrong version, or
/// [`Errno::BadMagic`] for a wrong magic, a dirty reserved field, a non-zero
/// name tail, or a non-UTF-8 name.
fn decode_frame(bytes: &[u8], magic: u32) -> Result<(u16, &str), Errno> {
    if bytes.len() < REQUEST_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if read_u32(bytes, OFF_MAGIC) != magic {
        return Err(Errno::BadMagic);
    }
    if read_u16(bytes, OFF_VERSION) != SERVICE_CONTROL_VERSION_V1 {
        return Err(Errno::AbiVersionUnsupported);
    }
    if read_u16(bytes, OFF_RESERVED) != 0 {
        return Err(Errno::BadMagic);
    }
    let name_len = read_u16(bytes, OFF_NAME_LEN) as usize;
    if name_len > SERVICE_MANIFEST_MAX_NAME_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    // The unused tail of the name field must be zero (canonical framing), so a
    // request has exactly one encoding.
    if bytes[OFF_NAME + name_len..REQUEST_LEN]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let name =
        core::str::from_utf8(&bytes[OFF_NAME..OFF_NAME + name_len]).map_err(|_| Errno::BadMagic)?;
    Ok((read_u16(bytes, OFF_OP), name))
}

/// Encoded length of a service-enrolment reply: the status word, the resulting
/// [`ServiceEnrolment`] byte, a `changed` flag, and a reserved tail. This is
/// also the enrolment endpoint's maximum reply size.
pub const ENROL_REPLY_LEN: usize = REPLY_STATUS_LEN + 4;

/// The runtime-control operation a request names.
///
/// A closed set: `start` and `stop` are the two runtime-lifecycle actions the
/// control endpoint brokers. Persistent enablement and status live elsewhere
/// (see the module docs), so they are deliberately absent — an unknown
/// discriminant fails the decode closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ServiceControlOp {
    /// Bring a registered, currently-down service up now (respecting the
    /// readiness conditions it requires; refused if one is unmet).
    Start = 1,
    /// Gracefully stop a running service (and, in reverse-dependency order,
    /// its dependents).
    Stop = 2,
}

impl ServiceControlOp {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a discriminant, failing closed on any unknown value.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Start),
            2 => Some(Self::Stop),
            _ => None,
        }
    }
}

/// A decoded service-control request: an operation and the name of the
/// service it targets, borrowed from the request buffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceControlRequest<'a> {
    /// The runtime-control operation to perform.
    pub op: ServiceControlOp,
    /// The target service name (bounded, validated as UTF-8; the manager
    /// re-validates it against its strict service-name policy).
    pub name: &'a str,
}

impl<'a> ServiceControlRequest<'a> {
    /// Encode this request into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold [`REQUEST_LEN`] bytes.
    /// * [`Errno::LengthOutOfRange`] if the name exceeds
    ///   [`SERVICE_MANIFEST_MAX_NAME_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        encode_frame(buf, SERVICE_CONTROL_MAGIC, self.op.as_u16(), self.name)
    }

    /// Decode a service-control request from `bytes`.
    ///
    /// Validates the whole frame up front (length, magic, version, a known
    /// operation, the reserved field, the name length against its bound, the
    /// unused name tail being zero, and the name being valid UTF-8), so an
    /// accepted request is well-formed and canonical and a malformed one
    /// fails closed.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a short frame or an over-bound
    /// `name_len`, [`Errno::BadMagic`] for a wrong magic or a dirty reserved
    /// field or non-zero name tail, [`Errno::AbiVersionUnsupported`] for a
    /// wrong version, [`Errno::OutOfRange`] for an unknown operation, or
    /// [`Errno::BadMagic`] for a non-UTF-8 name (malformed wire content).
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let (op, name) = decode_frame(bytes, SERVICE_CONTROL_MAGIC)?;
        let op = ServiceControlOp::from_u16(op).ok_or(Errno::OutOfRange)?;
        Ok(Self { op, name })
    }
}

/// The activation operation a request names.
///
/// A closed set of the two halves of one client's interest in a shared
/// service: `connect` declares it (activating the service if it is down and
/// parking the caller until it is ready), `disconnect` withdraws it. An
/// unknown discriminant fails the decode closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ServiceActivationOp {
    /// Take an interest in the named service, waiting for it to be ready.
    Connect = 1,
    /// Release a previously declared interest.
    Disconnect = 2,
}

impl ServiceActivationOp {
    /// The wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// The operation a wire discriminant names, or `None` for an unknown one.
    #[must_use]
    pub const fn from_u16(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Connect),
            2 => Some(Self::Disconnect),
            _ => None,
        }
    }
}

/// A decoded service-activation request: an operation and the name of the
/// service it targets, borrowed from the request buffer.
///
/// The request carries **no** client identity. The manager binds the call to
/// the kernel-attested origin of the caller, so a client can neither claim
/// another principal's connection nor forge the authority its connect is
/// checked against.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceActivationRequest<'a> {
    /// The activation operation to perform.
    pub op: ServiceActivationOp,
    /// The target service name (bounded, validated as UTF-8; the manager
    /// re-validates it against its strict service-name policy).
    pub name: &'a str,
}

impl<'a> ServiceActivationRequest<'a> {
    /// Encode this request into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold [`REQUEST_LEN`] bytes.
    /// * [`Errno::LengthOutOfRange`] if the name exceeds
    ///   [`SERVICE_MANIFEST_MAX_NAME_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        encode_frame(buf, SERVICE_ACTIVATION_MAGIC, self.op.as_u16(), self.name)
    }

    /// Decode a service-activation request from `bytes`, validating the whole
    /// frame up front exactly as [`ServiceControlRequest::decode`] does.
    ///
    /// # Errors
    ///
    /// As [`ServiceControlRequest::decode`], with [`Errno::BadMagic`] for a
    /// frame built for a sibling endpoint.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let (op, name) = decode_frame(bytes, SERVICE_ACTIVATION_MAGIC)?;
        let op = ServiceActivationOp::from_u16(op).ok_or(Errno::OutOfRange)?;
        Ok(Self { op, name })
    }
}

/// Encode a successful reply carrying the service's resulting `state`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`REPLY_LEN`] bytes.
pub fn encode_reply(buf: &mut [u8], state: ServiceState) -> Result<usize, Errno> {
    if buf.len() < REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    buf[..REPLY_LEN].fill(0);
    // Status word 0 = success; the state byte follows, reserved tail zero.
    buf[REPLY_STATUS_LEN] = state.as_u8();
    Ok(REPLY_LEN)
}

/// Encode a fail-closed error reply (status word only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    let neg = -err.as_i32();
    buf[..REPLY_STATUS_LEN].copy_from_slice(&neg.to_le_bytes());
    Ok(REPLY_STATUS_LEN)
}

/// Decode a reply, returning the service's resulting [`ServiceState`] on
/// success.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame; [`Errno::BadMagic`] for a
/// truncated success frame, an unknown state byte, or a dirty reserved tail
/// (wire corruption — fail closed); or [`Errno::BufferTooSmall`] if `reply`
/// is shorter than the status word.
pub fn decode_reply(reply: &[u8]) -> Result<ServiceState, Errno> {
    match reply_status(reply)? {
        0 => {
            if reply.len() < REPLY_LEN {
                return Err(Errno::BadMagic);
            }
            let state = ServiceState::from_u8(reply[REPLY_STATUS_LEN]).ok_or(Errno::BadMagic)?;
            if reply[REPLY_STATUS_LEN + 1..REPLY_LEN]
                .iter()
                .any(|&b| b != 0)
            {
                return Err(Errno::BadMagic);
            }
            Ok(state)
        }
        negative => Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic)),
    }
}

/// The persistent-enrolment operation a request names.
///
/// A closed set: `enable` and `disable` are the two enrolment changes the
/// enrolment endpoint brokers. An unknown discriminant fails the decode closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ServiceEnrolOp {
    /// Record the service as eligible to be brought up, and start it now if
    /// its readiness conditions allow.
    Enable = 1,
    /// Record the service as ineligible, and stop it if it is running.
    Disable = 2,
}

impl ServiceEnrolOp {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a discriminant, failing closed on any unknown value.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Enable),
            2 => Some(Self::Disable),
            _ => None,
        }
    }

    /// The enrolment this operation asks for.
    #[must_use]
    pub const fn wanted(self) -> ServiceEnrolment {
        match self {
            Self::Enable => ServiceEnrolment::Enabled,
            Self::Disable => ServiceEnrolment::Disabled,
        }
    }
}

/// A decoded service-enrolment request: an operation and the name of the
/// service it targets, borrowed from the request buffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceEnrolRequest<'a> {
    /// The enrolment change to record.
    pub op: ServiceEnrolOp,
    /// The target service name (bounded, validated as UTF-8; the manager
    /// re-validates it against its strict service-name policy).
    pub name: &'a str,
}

impl<'a> ServiceEnrolRequest<'a> {
    /// Encode this request into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold [`REQUEST_LEN`] bytes.
    /// * [`Errno::LengthOutOfRange`] if the name exceeds
    ///   [`SERVICE_MANIFEST_MAX_NAME_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        encode_frame(buf, SERVICE_ENROL_MAGIC, self.op.as_u16(), self.name)
    }

    /// Decode a service-enrolment request from `bytes`.
    ///
    /// Validates the whole frame up front, so an accepted request is
    /// well-formed and canonical and a malformed one fails closed.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a short frame or an over-bound
    /// `name_len`, [`Errno::BadMagic`] for a wrong magic (including a frame
    /// built for the control endpoint), a dirty reserved field, a non-zero
    /// name tail, or a non-UTF-8 name, [`Errno::AbiVersionUnsupported`] for a
    /// wrong version, or [`Errno::OutOfRange`] for an unknown operation.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let (op, name) = decode_frame(bytes, SERVICE_ENROL_MAGIC)?;
        let op = ServiceEnrolOp::from_u16(op).ok_or(Errno::OutOfRange)?;
        Ok(Self { op, name })
    }
}

/// Encode a successful enrolment reply: the resulting `enrolment` and whether
/// the request `changed` anything.
///
/// `changed` is what lets a tool distinguish "enabled it" from "it was already
/// enabled" without a second query, so an idempotent request reports honestly
/// instead of claiming work it did not do.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`ENROL_REPLY_LEN`] bytes.
pub fn encode_enrol_reply(
    buf: &mut [u8],
    enrolment: ServiceEnrolment,
    changed: bool,
) -> Result<usize, Errno> {
    if buf.len() < ENROL_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    buf[..ENROL_REPLY_LEN].fill(0);
    // Status word 0 = success; the enrolment byte and the flag follow.
    buf[REPLY_STATUS_LEN] = enrolment.as_u8();
    buf[REPLY_STATUS_LEN + 1] = u8::from(changed);
    Ok(ENROL_REPLY_LEN)
}

/// Decode an enrolment reply, returning the resulting enrolment and whether
/// the request changed anything.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame; [`Errno::BadMagic`] for a
/// truncated success frame, an unknown enrolment byte, a `changed` flag that is
/// neither 0 nor 1, or a dirty reserved tail (wire corruption — fail closed);
/// or [`Errno::BufferTooSmall`] if `reply` is shorter than the status word.
pub fn decode_enrol_reply(reply: &[u8]) -> Result<(ServiceEnrolment, bool), Errno> {
    let status = reply_status(reply)?;
    if status != 0 {
        return Err(Errno::try_from_status(status).unwrap_or(Errno::BadMagic));
    }
    if reply.len() < ENROL_REPLY_LEN {
        return Err(Errno::BadMagic);
    }
    let enrolment = ServiceEnrolment::from_u8(reply[REPLY_STATUS_LEN]).ok_or(Errno::BadMagic)?;
    let changed = match reply[REPLY_STATUS_LEN + 1] {
        0 => false,
        1 => true,
        _ => return Err(Errno::BadMagic),
    };
    if reply[REPLY_STATUS_LEN + 2..ENROL_REPLY_LEN]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    Ok((enrolment, changed))
}

/// Encoded length of a lifecycle-notice reply: the status word, the
/// resulting [`ServiceState`] byte, a reserved tail, and the watchdog
/// interval the manager holds for the sender. This is also the notice
/// endpoint's maximum reply size.
pub const NOTICE_REPLY_LEN: usize = REPLY_LEN + Duration64::WIRE_LEN;

/// Wire offset of the watchdog interval in a notice reply.
const OFF_NOTICE_WATCHDOG: usize = REPLY_LEN;

/// Encode a successful notice reply: the sender's resulting `state` and the
/// `watchdog` interval the manager is holding it to
/// ([`Duration64::ZERO`] when it is not watched).
///
/// The interval rides the answer the service is already waiting for, so a
/// service learns its renewal cadence from the authority that enforces it
/// rather than from a second copy of its own unit metadata — the manager
/// and the service can never disagree about it, and a service whose
/// watchdog is later disarmed learns that from its next renewal.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`NOTICE_REPLY_LEN`] bytes.
pub fn encode_notice_reply(
    buf: &mut [u8],
    state: ServiceState,
    watchdog: Duration64,
) -> Result<usize, Errno> {
    if buf.len() < NOTICE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    buf[..NOTICE_REPLY_LEN].fill(0);
    buf[REPLY_STATUS_LEN] = state.as_u8();
    buf[OFF_NOTICE_WATCHDOG..NOTICE_REPLY_LEN].copy_from_slice(&watchdog.to_le_bytes());
    Ok(NOTICE_REPLY_LEN)
}

/// Decode a notice reply, returning the sender's resulting state and the
/// watchdog interval the manager holds it to.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame; [`Errno::BadMagic`] for a
/// truncated success frame, an unknown state byte, a dirty reserved tail, or
/// a negative interval (wire corruption — fail closed); or
/// [`Errno::BufferTooSmall`] if `reply` is shorter than the status word.
pub fn decode_notice_reply(reply: &[u8]) -> Result<(ServiceState, Duration64), Errno> {
    let status = reply_status(reply)?;
    if status != 0 {
        return Err(Errno::try_from_status(status).unwrap_or(Errno::BadMagic));
    }
    if reply.len() < NOTICE_REPLY_LEN {
        return Err(Errno::BadMagic);
    }
    let state = ServiceState::from_u8(reply[REPLY_STATUS_LEN]).ok_or(Errno::BadMagic)?;
    if reply[REPLY_STATUS_LEN + 1..OFF_NOTICE_WATCHDOG]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let watchdog = Duration64::from_bytes(&reply[OFF_NOTICE_WATCHDOG..NOTICE_REPLY_LEN])
        .map_err(|_| Errno::BadMagic)?;
    // A renewal cadence derived from a negative interval would be
    // meaningless, so it is refused rather than clamped.
    if watchdog < Duration64::ZERO {
        return Err(Errno::BadMagic);
    }
    Ok((state, watchdog))
}

/// The status word of a reply frame, shared by both endpoints' decoders.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` is shorter than the status word.
fn reply_status(reply: &[u8]) -> Result<i32, Errno> {
    if reply.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    Ok(i32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(op: ServiceControlOp, name: &str) {
        let req = ServiceControlRequest { op, name };
        let mut buf = [0u8; REQUEST_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, REQUEST_LEN);
        assert_eq!(ServiceControlRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn request_round_trips_both_ops() {
        round_trip(ServiceControlOp::Start, "fontd");
        round_trip(ServiceControlOp::Stop, "netstack");
    }

    #[test]
    fn empty_name_round_trips() {
        round_trip(ServiceControlOp::Start, "");
    }

    #[test]
    fn endpoint_and_magic_are_frozen() {
        assert_eq!(SERVICE_CONTROL_ENDPOINT, 0x5356_4300);
        assert!(crate::ipc::is_reserved_endpoint(SERVICE_CONTROL_ENDPOINT));
        assert_eq!(SERVICE_CONTROL_MAGIC, u32::from_le_bytes(*b"SVCC"));
        assert_eq!(SERVICE_CONTROL_VERSION_V1, 1);
    }

    #[test]
    fn op_discriminants_round_trip_and_reject_unknown() {
        assert_eq!(ServiceControlOp::Start.as_u16(), 1);
        assert_eq!(ServiceControlOp::Stop.as_u16(), 2);
        assert_eq!(ServiceControlOp::from_u16(1), Some(ServiceControlOp::Start));
        assert_eq!(ServiceControlOp::from_u16(2), Some(ServiceControlOp::Stop));
        assert_eq!(ServiceControlOp::from_u16(0), None);
        assert_eq!(ServiceControlOp::from_u16(3), None);
    }

    #[test]
    fn encode_rejects_a_short_buffer_and_an_over_bound_name() {
        let req = ServiceControlRequest {
            op: ServiceControlOp::Start,
            name: "x",
        };
        let mut small = [0u8; REQUEST_LEN - 1];
        assert_eq!(req.encode(&mut small), Err(Errno::BufferTooSmall));

        let long = "a".repeat(SERVICE_MANIFEST_MAX_NAME_LEN + 1);
        let over = ServiceControlRequest {
            op: ServiceControlOp::Start,
            name: &long,
        };
        let mut buf = [0u8; REQUEST_LEN];
        assert_eq!(over.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn decode_fails_closed_on_malformed_framing() {
        let base = ServiceControlRequest {
            op: ServiceControlOp::Start,
            name: "fontd",
        };
        let mut buf = [0u8; REQUEST_LEN];
        base.encode(&mut buf).expect("encodes");

        // Truncated.
        assert_eq!(
            ServiceControlRequest::decode(&buf[..REQUEST_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );

        // Bad magic.
        let mut bad = buf;
        bad[OFF_MAGIC] ^= 0xFF;
        assert_eq!(ServiceControlRequest::decode(&bad), Err(Errno::BadMagic));

        // Wrong version.
        let mut bad = buf;
        put_u16(&mut bad, OFF_VERSION, 2);
        assert_eq!(
            ServiceControlRequest::decode(&bad),
            Err(Errno::AbiVersionUnsupported)
        );

        // Unknown op.
        let mut bad = buf;
        put_u16(&mut bad, OFF_OP, 9);
        assert_eq!(ServiceControlRequest::decode(&bad), Err(Errno::OutOfRange));

        // Dirty reserved field.
        let mut bad = buf;
        put_u16(&mut bad, OFF_RESERVED, 1);
        assert_eq!(ServiceControlRequest::decode(&bad), Err(Errno::BadMagic));

        // Over-bound name length.
        let mut bad = buf;
        put_u16(
            &mut bad,
            OFF_NAME_LEN,
            u16::try_from(SERVICE_MANIFEST_MAX_NAME_LEN + 1).expect("fits u16"),
        );
        assert_eq!(
            ServiceControlRequest::decode(&bad),
            Err(Errno::LengthOutOfRange)
        );

        // Non-zero tail after the name.
        let mut bad = buf;
        bad[REQUEST_LEN - 1] = 1;
        assert_eq!(ServiceControlRequest::decode(&bad), Err(Errno::BadMagic));
    }

    #[test]
    fn decode_rejects_a_non_utf8_name() {
        let mut buf = [0u8; REQUEST_LEN];
        put_u32(&mut buf, OFF_MAGIC, SERVICE_CONTROL_MAGIC);
        put_u16(&mut buf, OFF_VERSION, SERVICE_CONTROL_VERSION_V1);
        put_u16(&mut buf, OFF_OP, ServiceControlOp::Start.as_u16());
        put_u16(&mut buf, OFF_NAME_LEN, 1);
        buf[OFF_NAME] = 0xFF; // not valid UTF-8
        assert_eq!(ServiceControlRequest::decode(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn enrol_request_round_trips_both_ops() {
        for op in [ServiceEnrolOp::Enable, ServiceEnrolOp::Disable] {
            let req = ServiceEnrolRequest { op, name: "timed" };
            let mut buf = [0u8; REQUEST_LEN];
            let n = req.encode(&mut buf).expect("encodes");
            assert_eq!(n, REQUEST_LEN);
            assert_eq!(ServiceEnrolRequest::decode(&buf[..n]), Ok(req));
        }
    }

    #[test]
    fn the_two_endpoints_frames_are_not_interchangeable() {
        // A control frame must not decode as an enrolment request and vice
        // versa: the magic separates them before any operation is classified,
        // so an operation cannot be smuggled onto the endpoint whose authority
        // it was not meant for.
        let mut control = [0u8; REQUEST_LEN];
        ServiceControlRequest {
            op: ServiceControlOp::Stop,
            name: "timed",
        }
        .encode(&mut control)
        .expect("encodes");
        assert_eq!(ServiceEnrolRequest::decode(&control), Err(Errno::BadMagic));

        let mut enrol = [0u8; REQUEST_LEN];
        ServiceEnrolRequest {
            op: ServiceEnrolOp::Disable,
            name: "timed",
        }
        .encode(&mut enrol)
        .expect("encodes");
        assert_eq!(ServiceControlRequest::decode(&enrol), Err(Errno::BadMagic));
        assert_ne!(SERVICE_CONTROL_MAGIC, SERVICE_ENROL_MAGIC);
    }

    #[test]
    fn enrol_endpoint_and_magic_are_frozen() {
        assert_eq!(SERVICE_ENROL_ENDPOINT, 0x5356_4500);
        assert_ne!(SERVICE_ENROL_ENDPOINT, SERVICE_CONTROL_ENDPOINT);
        assert!(crate::ipc::is_reserved_endpoint(SERVICE_ENROL_ENDPOINT));
        assert_eq!(SERVICE_ENROL_MAGIC, u32::from_le_bytes(*b"SVCE"));
    }

    #[test]
    fn enrol_op_discriminants_round_trip_and_reject_unknown() {
        assert_eq!(ServiceEnrolOp::Enable.as_u16(), 1);
        assert_eq!(ServiceEnrolOp::Disable.as_u16(), 2);
        assert_eq!(ServiceEnrolOp::from_u16(1), Some(ServiceEnrolOp::Enable));
        assert_eq!(ServiceEnrolOp::from_u16(2), Some(ServiceEnrolOp::Disable));
        assert_eq!(ServiceEnrolOp::from_u16(0), None);
        assert_eq!(ServiceEnrolOp::from_u16(3), None);
        assert_eq!(ServiceEnrolOp::Enable.wanted(), ServiceEnrolment::Enabled);
        assert_eq!(ServiceEnrolOp::Disable.wanted(), ServiceEnrolment::Disabled);
    }

    #[test]
    fn enrol_reply_round_trips_and_fails_closed() {
        for (enrolment, changed) in [
            (ServiceEnrolment::Enabled, true),
            (ServiceEnrolment::Enabled, false),
            (ServiceEnrolment::Disabled, true),
            (ServiceEnrolment::Disabled, false),
        ] {
            let mut buf = [0u8; ENROL_REPLY_LEN];
            let n = encode_enrol_reply(&mut buf, enrolment, changed).expect("encodes");
            assert_eq!(n, ENROL_REPLY_LEN);
            assert_eq!(decode_enrol_reply(&buf[..n]), Ok((enrolment, changed)));
        }

        // An error frame carries its errno.
        let mut buf = [0u8; ENROL_REPLY_LEN];
        let n = encode_error_reply(&mut buf, Errno::NotFound).expect("encodes");
        assert_eq!(decode_enrol_reply(&buf[..n]), Err(Errno::NotFound));

        // Success status but truncated (no enrolment byte).
        let mut buf = [0u8; REPLY_STATUS_LEN];
        buf.copy_from_slice(&0i32.to_le_bytes());
        assert_eq!(decode_enrol_reply(&buf), Err(Errno::BadMagic));

        // Unknown enrolment byte, a non-boolean flag, and a dirty tail each
        // fail closed rather than being read as a plausible answer.
        let mut buf = [0u8; ENROL_REPLY_LEN];
        buf[REPLY_STATUS_LEN] = 0xEE;
        assert_eq!(decode_enrol_reply(&buf), Err(Errno::BadMagic));

        let mut buf = [0u8; ENROL_REPLY_LEN];
        encode_enrol_reply(&mut buf, ServiceEnrolment::Enabled, false).expect("encodes");
        buf[REPLY_STATUS_LEN + 1] = 2;
        assert_eq!(decode_enrol_reply(&buf), Err(Errno::BadMagic));

        let mut buf = [0u8; ENROL_REPLY_LEN];
        encode_enrol_reply(&mut buf, ServiceEnrolment::Enabled, true).expect("encodes");
        buf[ENROL_REPLY_LEN - 1] = 1;
        assert_eq!(decode_enrol_reply(&buf), Err(Errno::BadMagic));

        // `i32::MIN` status word: negating it would overflow, so fail closed.
        let mut buf = [0u8; ENROL_REPLY_LEN];
        buf[..REPLY_STATUS_LEN].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(decode_enrol_reply(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn enrolment_discriminants_and_words_round_trip() {
        for e in [ServiceEnrolment::Enabled, ServiceEnrolment::Disabled] {
            assert_eq!(ServiceEnrolment::from_u8(e.as_u8()), Some(e));
            assert_eq!(ServiceEnrolment::from_name(e.as_str()), Some(e));
        }
        assert_eq!(ServiceEnrolment::from_u8(0), None);
        assert_eq!(ServiceEnrolment::from_u8(3), None);
        assert_eq!(ServiceEnrolment::from_name("off"), None);
        assert!(ServiceEnrolment::Enabled.is_enabled());
        assert!(!ServiceEnrolment::Disabled.is_enabled());
    }

    #[test]
    fn reply_round_trips_ok_and_error() {
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_reply(&mut buf, ServiceState::Starting).expect("encodes");
        assert_eq!(n, REPLY_LEN);
        assert_eq!(decode_reply(&buf[..n]), Ok(ServiceState::Starting));

        let mut buf = [0u8; REPLY_LEN];
        let n = encode_error_reply(&mut buf, Errno::PermissionDenied).expect("encodes");
        assert_eq!(decode_reply(&buf[..n]), Err(Errno::PermissionDenied));
    }

    #[test]
    fn reply_decode_fails_closed() {
        // Success status but truncated (no state byte).
        let mut buf = [0u8; REPLY_STATUS_LEN];
        buf.copy_from_slice(&0i32.to_le_bytes());
        assert_eq!(decode_reply(&buf), Err(Errno::BadMagic));

        // Success status, unknown state byte.
        let mut buf = [0u8; REPLY_LEN];
        buf[REPLY_STATUS_LEN] = 0xEE;
        assert_eq!(decode_reply(&buf), Err(Errno::BadMagic));

        // Success status, valid state, dirty reserved tail.
        let mut buf = [0u8; REPLY_LEN];
        encode_reply(&mut buf, ServiceState::Ready).expect("encodes");
        buf[REPLY_LEN - 1] = 1;
        assert_eq!(decode_reply(&buf), Err(Errno::BadMagic));

        // Corrupt (unknown) negative status.
        let mut buf = [0u8; REPLY_LEN];
        buf[..REPLY_STATUS_LEN].copy_from_slice(&(-9_999i32).to_le_bytes());
        assert_eq!(decode_reply(&buf), Err(Errno::BadMagic));

        // `i32::MIN` status word: negating it would overflow, so the decoder
        // must fail closed rather than panic (a fuzz-found regression).
        let mut buf = [0u8; REPLY_LEN];
        buf[..REPLY_STATUS_LEN].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(decode_reply(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn an_activation_request_round_trips() {
        for op in [
            ServiceActivationOp::Connect,
            ServiceActivationOp::Disconnect,
        ] {
            let mut buf = [0u8; REQUEST_LEN];
            let written = ServiceActivationRequest { op, name: "fontd" }
                .encode(&mut buf)
                .expect("encodes");
            assert_eq!(written, REQUEST_LEN);
            assert_eq!(
                ServiceActivationRequest::decode(&buf),
                Ok(ServiceActivationRequest { op, name: "fontd" })
            );
        }
    }

    #[test]
    fn activation_discriminants_are_pinned_and_closed() {
        assert_eq!(ServiceActivationOp::Connect.as_u16(), 1);
        assert_eq!(ServiceActivationOp::Disconnect.as_u16(), 2);
        assert_eq!(
            ServiceActivationOp::from_u16(1),
            Some(ServiceActivationOp::Connect)
        );
        assert_eq!(
            ServiceActivationOp::from_u16(2),
            Some(ServiceActivationOp::Disconnect)
        );
        assert_eq!(ServiceActivationOp::from_u16(0), None);
        assert_eq!(ServiceActivationOp::from_u16(3), None);
    }

    /// Each endpoint's decoder refuses the other two's frames on the magic,
    /// before any operation is classified — so a client frame can never be
    /// mistaken for an administrator's, whichever endpoint it reaches.
    #[test]
    fn a_frame_built_for_one_endpoint_is_refused_by_the_others() {
        let mut control = [0u8; REQUEST_LEN];
        ServiceControlRequest {
            op: ServiceControlOp::Start,
            name: "fontd",
        }
        .encode(&mut control)
        .expect("encodes");
        let mut enrol = [0u8; REQUEST_LEN];
        ServiceEnrolRequest {
            op: ServiceEnrolOp::Enable,
            name: "fontd",
        }
        .encode(&mut enrol)
        .expect("encodes");
        let mut activation = [0u8; REQUEST_LEN];
        ServiceActivationRequest {
            op: ServiceActivationOp::Connect,
            name: "fontd",
        }
        .encode(&mut activation)
        .expect("encodes");

        assert_eq!(
            ServiceActivationRequest::decode(&control),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            ServiceActivationRequest::decode(&enrol),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            ServiceControlRequest::decode(&activation),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            ServiceEnrolRequest::decode(&activation),
            Err(Errno::BadMagic)
        );
    }

    /// The activation endpoint is reserved, so the kernel gates who may bind
    /// it — a squatter cannot stand between a client and the manager.
    #[test]
    fn the_activation_endpoint_is_reserved_and_distinct() {
        assert!(crate::ipc::is_reserved_endpoint(
            SERVICE_ACTIVATION_ENDPOINT
        ));
        assert_ne!(SERVICE_ACTIVATION_ENDPOINT, SERVICE_CONTROL_ENDPOINT);
        assert_ne!(SERVICE_ACTIVATION_ENDPOINT, SERVICE_ENROL_ENDPOINT);
        assert_ne!(SERVICE_ACTIVATION_MAGIC, SERVICE_CONTROL_MAGIC);
        assert_ne!(SERVICE_ACTIVATION_MAGIC, SERVICE_ENROL_MAGIC);
    }

    /// The notice endpoint is reserved and distinct from its three siblings,
    /// and a notice frame is refused by every name-carrying decoder: the
    /// notice grammar names no service, so it must never be readable as a
    /// request that does.
    #[test]
    fn the_notice_endpoint_is_reserved_and_its_frame_is_not_a_request() {
        assert!(crate::ipc::is_reserved_endpoint(SERVICE_NOTICE_ENDPOINT));
        for sibling in [
            SERVICE_CONTROL_ENDPOINT,
            SERVICE_ENROL_ENDPOINT,
            SERVICE_ACTIVATION_ENDPOINT,
        ] {
            assert_ne!(SERVICE_NOTICE_ENDPOINT, sibling);
        }
        // A notice is shorter than the shared request frame, so a decoder
        // reading one refuses it on length before it reaches a magic.
        let notice = crate::ServiceNotice::Lifecycle(crate::LifecycleSignal::Ready).to_le_bytes();
        assert!(notice.len() < REQUEST_LEN);
        assert_eq!(
            ServiceControlRequest::decode(&notice),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ServiceEnrolRequest::decode(&notice),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ServiceActivationRequest::decode(&notice),
            Err(Errno::LengthOutOfRange)
        );
        // And the reverse: a request frame is not a notice. It is long
        // enough to reach the notice decoder's magic check, which refuses
        // it, so an operation can never arrive as an announcement.
        let mut request = [0u8; REQUEST_LEN];
        ServiceControlRequest {
            op: ServiceControlOp::Stop,
            name: "fontd",
        }
        .encode(&mut request)
        .expect("encodes");
        assert_eq!(
            crate::ServiceNotice::from_bytes(&request),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn notice_reply_round_trips_state_and_watchdog() {
        let mut buf = [0u8; NOTICE_REPLY_LEN];
        let interval = Duration64::from_secs(30);
        let len = encode_notice_reply(&mut buf, ServiceState::Running, interval).expect("encodes");
        assert_eq!(len, NOTICE_REPLY_LEN);
        assert_eq!(
            decode_notice_reply(&buf),
            Ok((ServiceState::Running, interval))
        );

        // An unwatched service reports a zero interval, which is what tells
        // its runtime to send no further renewals.
        let len =
            encode_notice_reply(&mut buf, ServiceState::Ready, Duration64::ZERO).expect("encodes");
        assert_eq!(
            decode_notice_reply(&buf[..len]),
            Ok((ServiceState::Ready, Duration64::ZERO))
        );
    }

    #[test]
    fn notice_reply_decode_fails_closed() {
        let mut good = [0u8; NOTICE_REPLY_LEN];
        encode_notice_reply(&mut good, ServiceState::Running, Duration64::from_secs(5))
            .expect("encodes");

        assert_eq!(
            decode_notice_reply(&good[..NOTICE_REPLY_LEN - 1]),
            Err(Errno::BadMagic)
        );

        let mut bad_state = good;
        bad_state[REPLY_STATUS_LEN] = 200;
        assert_eq!(decode_notice_reply(&bad_state), Err(Errno::BadMagic));

        let mut dirty_tail = good;
        dirty_tail[REPLY_STATUS_LEN + 1] = 1;
        assert_eq!(decode_notice_reply(&dirty_tail), Err(Errno::BadMagic));

        // A negative interval would yield a meaningless renewal cadence.
        let mut negative = good;
        negative[OFF_NOTICE_WATCHDOG..OFF_NOTICE_WATCHDOG + 8]
            .copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(decode_notice_reply(&negative), Err(Errno::BadMagic));

        // An error frame surfaces its own refusal, never a fabricated state.
        let mut err = [0u8; NOTICE_REPLY_LEN];
        let len = encode_error_reply(&mut err, Errno::NotFound).expect("encodes");
        assert_eq!(decode_notice_reply(&err[..len]), Err(Errno::NotFound));
    }
}
