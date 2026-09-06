//! The Switchboard tray-summary IPC protocol (`plans/NEW-TASKBAR.md` T9/T10):
//! the reserved rendezvous the desktop session binds and the fixed-width,
//! fail-closed request the Switchboard monitor service publishes a compact
//! tray-signal summary through.
//!
//! The Switchboard is a dedicated, capability-sized system service (its own
//! signed bundle, `userland/gui/switchboard`) that samples the live system
//! through the System Information API and publishes an at-a-glance summary —
//! active background jobs, recovery-candidate count, overall CPU load,
//! resource pressure (how many distinct resources are pressured, and the
//! single dominant one that drives the icon's rail), and the busiest task
//! (for the taskbar icon's hover readout) — to the desktop session, which
//! drives the always-right-most Switchboard taskbar icon from it.
//! Publication is event-driven (on change), never polled.
//!
//! The session keys the summary to the kernel-attested identity of the
//! Switchboard instance that published it
//! ([`crate::origin`] / `call_peer_origin`), never to anything claimed on
//! the wire — attested per request, exactly like the notification channel's
//! producer identity, and unrestricted-sender at bind (the endpoint itself
//! carries no per-request authority check beyond that attestation).
//!
//! The top-task name is producer-supplied **display text** validated at
//! construction and again at decode (bounded UTF-8, no control characters,
//! never sanitised): it names a process for a hover readout and carries no
//! authority, exactly like a notification title or a window title.
//!
//! Every operation is a fixed-width [`SwitchboardRequest`]. A
//! [`SwitchboardRequest::PublishSummary`] is answered with the longer
//! publish frame ([`encode_publish_reply`] / [`decode_publish_reply`]),
//! which carries the serving session's own kernel-attested [`ProcId`]; the
//! remaining operations are answered with the shared status frame
//! ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]) — success, or a typed refusal.
//! Every decode fails closed: an unknown magic, version, or operation, an
//! out-of-range pressure kind, pressured-resource count, or permille
//! fraction, an over-long or malformed top-task name, a zero owner id, or a
//! dirty reserved field refuses rather than guessing.
//!
//! # The two directions (`plans/NEW-TASKBAR.md` T11)
//!
//! The monitor renders the desktop's window stack but owns none of it, so
//! the actions its panel offers over *other* processes' windows —
//! switch-to-window and restart-a-hung-app — are requested of the session
//! rather than performed by the service:
//! [`SwitchboardRequest::ActivateOwner`] raises an owner's front window and
//! [`SwitchboardRequest::RestartOwner`] re-launches it through the
//! session's own attested launch path. The session honours either only from
//! the attested origin of the Switchboard instance it launched, and
//! validates the named owner against its own live window registry, so a
//! stale or invented owner id fails closed instead of reaching a stranger's
//! process.
//!
//! The reverse direction is a per-instance command mailbox
//! ([`command_endpoint_for`]) the service binds under its own task id: the
//! session sends [`SwitchboardCommand::OpenPanel`] when the user opens the
//! tray icon, and [`SwitchboardCommand::SeatReport`] to hand over the one
//! fact only the session holds — which window owners have stopped draining
//! their event mailbox and are therefore unresponsive. The service
//! authenticates every command against the session [`ProcId`] the publish
//! reply attested, never a wire claim, and joins the reported owner ids
//! against the process list it already samples rather than trusting names
//! from the wire.

use crate::bounded_text::BoundedText;
use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::origin::{ProcId, PROC_ID_LEN};
use crate::power::PowerAction;
use crate::Errno;

/// Reserved well-known call-endpoint id of the desktop session's
/// Switchboard tray-summary service (`"SW"` ASCII hex-spelled prefix,
/// mirroring [`crate::notify_ipc::NOTIFY_ENDPOINT`]'s convention). Served by
/// the desktop session, not the Switchboard process itself: the session owns
/// the seat and the taskbar icon the summary drives. Like the window and
/// notification rendezvous it is **seat-scoped**
/// ([`crate::ipc::is_reserved_endpoint`],
/// [`crate::ipc::is_seat_scoped_endpoint`]): the kernel authorises its bind
/// either by `CAP_IPC_BIND_PRIVILEGED` or by the caller's kernel-attested
/// **live seat lease**, so the session that owns the seat is the only
/// unprivileged caller that can serve it. The bind itself is
/// unrestricted-sender (any process may call in without a per-endpoint
/// grant); the Switchboard instance that published a summary is instead
/// attested per request from the kernel-provided caller identity
/// (`call_peer_origin`), never from the wire. A squatter claiming the
/// rendezvous first could feed the taskbar icon a fabricated summary, so an
/// unentitled bind fails closed.
pub const SWITCHBOARD_ENDPOINT: u64 = 0x5357_1001;

/// Magic number identifying a Switchboard tray-summary request (`"SWB1"`
/// little-endian).
pub const SWITCHBOARD_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"SWB1");

/// The `switchboard-v1` protocol version.
pub const SWITCHBOARD_VERSION_V1: u16 = 1;

/// Maximum request, in bytes, the [`SWITCHBOARD_ENDPOINT`] accepts: exactly
/// one fixed-width [`SwitchboardRequest`].
pub const SWITCHBOARD_MAX_REQUEST: usize = SwitchboardRequest::WIRE_LEN;

/// Maximum encoded length, in bytes, of a tray summary's top-task name.
///
/// A validation bound, not a capacity ([`crate::rlimit`] governs
/// capacities): matches the System Information API's own process-name width
/// ([`crate::sysinfo::PROCESS_NAME_MAX`]), since the name is that same
/// process name, just carried on a different wire.
pub const TRAY_TASK_NAME_MAX: usize = 32;

/// A validated top-task name: at least one and at most
/// [`TRAY_TASK_NAME_MAX`] bytes of well-formed UTF-8 with no control
/// characters.
///
/// Built on the shared [`BoundedText`] validator (`crate::bounded_text`), so
/// its construction and decode rules are identical to the notification
/// channel's title and body. `MIN` is `1`, never `0`: an empty name has no
/// meaning here — the absence of a busiest task is
/// [`TraySummary::top_task`] being `None`, never a [`TrayTask`] whose name
/// is empty.
pub type TrayTaskName = BoundedText<1, TRAY_TASK_NAME_MAX>;

/// Which resource a [`TrayPressure`] names.
///
/// A closed set mirroring the shared control vocabulary's own pressure
/// kinds (`lib/controls`' `PressureKind`), but defined independently here:
/// `lib/abi` cannot depend on `lib/controls` (the ABI is the
/// platform-neutral, dependency-free foundation every layer above it builds
/// on). Wire byte `0` is reserved for "no pressure" in the enclosing frame
/// and is never a valid discriminant; decoding any byte outside `1..=6`
/// fails closed with [`Errno::OutOfRange`] rather than guessing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrayPressureKind {
    /// Compute saturation.
    Cpu,
    /// Memory pressure.
    Memory,
    /// Storage throughput.
    Disk,
    /// Network transfer / remote I/O.
    Network,
    /// Power / battery pressure.
    Power,
    /// Thermal pressure.
    Thermal,
}

/// Wire discriminant of [`TrayPressureKind::Cpu`].
const PRESSURE_KIND_CPU: u8 = 1;
/// Wire discriminant of [`TrayPressureKind::Memory`].
const PRESSURE_KIND_MEMORY: u8 = 2;
/// Wire discriminant of [`TrayPressureKind::Disk`].
const PRESSURE_KIND_DISK: u8 = 3;
/// Wire discriminant of [`TrayPressureKind::Network`].
const PRESSURE_KIND_NETWORK: u8 = 4;
/// Wire discriminant of [`TrayPressureKind::Power`].
const PRESSURE_KIND_POWER: u8 = 5;
/// Wire discriminant of [`TrayPressureKind::Thermal`].
const PRESSURE_KIND_THERMAL: u8 = 6;
/// Wire sentinel meaning "no pressure" in the enclosing frame; never a valid
/// [`TrayPressureKind`] discriminant.
const PRESSURE_KIND_NONE: u8 = 0;

impl TrayPressureKind {
    /// The wire discriminant of this pressure kind (a non-zero byte; zero
    /// is reserved so a zeroed frame never decodes as a valid kind).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Cpu => PRESSURE_KIND_CPU,
            Self::Memory => PRESSURE_KIND_MEMORY,
            Self::Disk => PRESSURE_KIND_DISK,
            Self::Network => PRESSURE_KIND_NETWORK,
            Self::Power => PRESSURE_KIND_POWER,
            Self::Thermal => PRESSURE_KIND_THERMAL,
        }
    }

    /// Decode a pressure kind from its wire discriminant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any byte outside the closed set, including
    /// the reserved `0` (fail closed on a corrupt or hostile frame).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            PRESSURE_KIND_CPU => Ok(Self::Cpu),
            PRESSURE_KIND_MEMORY => Ok(Self::Memory),
            PRESSURE_KIND_DISK => Ok(Self::Disk),
            PRESSURE_KIND_NETWORK => Ok(Self::Network),
            PRESSURE_KIND_POWER => Ok(Self::Power),
            PRESSURE_KIND_THERMAL => Ok(Self::Thermal),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Number of kinds in the closed [`TrayPressureKind`] set — the ceiling of
/// a summary's pressured-resource count ([`TrayPressureCount`]).
pub const TRAY_PRESSURE_KIND_COUNT: u8 = 6;

/// A validated fraction in permille (`0..=1000`).
///
/// Constructed through [`TrayPermille::new`], which fails closed on
/// out-of-range input rather than clamping: a caller can never smuggle a
/// fraction beyond full past the constructor, and a decoder never has to
/// defend against one downstream.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TrayPermille(u16);

impl TrayPermille {
    /// No measured load (`0` permille).
    pub const ZERO: Self = Self(0);
    /// Fully saturated (`1000` permille).
    pub const FULL: Self = Self(1000);

    /// Build a fraction from its permille value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — `permille` exceeds `1000`.
    pub const fn new(permille: u16) -> Result<Self, Errno> {
        if permille > 1000 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(permille))
    }

    /// The raw on-wire value (`0..=1000`).
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

/// A validated count of distinct resources currently measured under
/// pressure (`1..=`[`TRAY_PRESSURE_KIND_COUNT`]), the dominant one
/// included. It drives the tray icon's numeric badge.
///
/// Only a summary that names a pressure carries a count, so zero is not
/// representable: "no pressure at all" is [`TraySummary::pressure`] being
/// `None`, whose wire encoding zeroes the count byte alongside the kind
/// and level. That shape makes the badge invariant — zero exactly when no
/// pressure is shown, at least one otherwise — a property of the types
/// rather than a rule every consumer re-checks.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TrayPressureCount(u8);

impl TrayPressureCount {
    /// Exactly one pressured resource — the dominant one itself.
    pub const ONE: Self = Self(1);

    /// Build a count from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — `count` is zero or exceeds
    /// [`TRAY_PRESSURE_KIND_COUNT`] (fail closed: a present pressure names
    /// at least itself, and the closed kind set bounds how many distinct
    /// resources can be pressured at once).
    pub const fn new(count: u8) -> Result<Self, Errno> {
        if count == 0 || count > TRAY_PRESSURE_KIND_COUNT {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(count))
    }

    /// The raw on-wire value (`1..=`[`TRAY_PRESSURE_KIND_COUNT`]).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }
}

/// The measured resource pressure a [`TraySummary`] names.
///
/// A summary surfaces one *dominant* pressure — the Switchboard sampler
/// picks the most severe to drive the tray icon's pressure rail, exactly
/// as the shared control vocabulary's own pressure state is a single
/// dominant signal — plus the count of distinct pressured resources behind
/// it, which drives the icon's numeric badge (for example `2` when both
/// CPU and memory are pressured while the rail shows the worse of the
/// two).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TrayPressure {
    /// Which resource dominates.
    pub kind: TrayPressureKind,
    /// How severe the dominant pressure is.
    pub level: TrayPermille,
    /// How many distinct resources are under pressure, this one included.
    pub count: TrayPressureCount,
}

/// The busiest task, for the tray icon's hover readout ("Hover to preview
/// top task").
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TrayTask {
    /// The task's display name.
    pub name: TrayTaskName,
    /// Its CPU share over the last sample interval.
    pub cpu_permille: TrayPermille,
}

/// The compact tray-signal summary a Switchboard instance publishes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TraySummary {
    /// Count of active background jobs.
    pub jobs: u16,
    /// Count of recovery candidates (e.g. stopped processes).
    pub recovery: u16,
    /// Overall CPU busy fraction over the last sample interval.
    pub cpu_busy_permille: TrayPermille,
    /// The measured resource pressure, if any: the dominant signal plus
    /// the pressured-resource count.
    pub pressure: Option<TrayPressure>,
    /// The busiest task, if any is known.
    pub top_task: Option<TrayTask>,
    /// Whether the publishing Switchboard instance has attested that it
    /// holds `CAP_SYSTEM_POWER`. The taskbar's system menu fails closed on
    /// its Restart/Shut Down rows whenever this is `false` — including
    /// before the first publish, when no summary exists at all — so a
    /// service that never requested (or was refused) the capability never
    /// lets the desktop offer an action it cannot perform.
    pub power_capable: bool,
}

/// One Switchboard channel operation the desktop session serves
/// (`plans/NEW-TASKBAR.md` T9–T11).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardRequest {
    /// Publish (replacing any prior) the caller's tray-signal summary.
    PublishSummary {
        /// The compact summary to show on the tray icon.
        summary: TraySummary,
    },
    /// Raise and focus the front window of the window owner named by its
    /// kernel task id — the panel's switch-to-window action, applied by the
    /// session because it alone owns the window stack.
    ActivateOwner {
        /// The owning app's kernel task id.
        owner: u64,
    },
    /// Re-launch the window owner named by its kernel task id through the
    /// session's attested launch path — the panel's restart action for an
    /// unresponsive app, applied by the session because it alone holds the
    /// bundle each window was launched from.
    RestartOwner {
        /// The owning app's kernel task id.
        owner: u64,
    },
}

/// Wire operation discriminant of [`SwitchboardRequest::PublishSummary`].
const OP_PUBLISH_SUMMARY: u16 = 1;
/// Wire operation discriminant of [`SwitchboardRequest::ActivateOwner`].
const OP_ACTIVATE_OWNER: u16 = 2;
/// Wire operation discriminant of [`SwitchboardRequest::RestartOwner`].
const OP_RESTART_OWNER: u16 = 3;

/// Byte offset of an owner-directed operation's kernel task id.
const OWNER_OFFSET: usize = 8;
/// First byte after an owner-directed operation's payload; everything from
/// here to the end of the frame is reserved and must be zero.
const OWNER_TAIL_OFFSET: usize = OWNER_OFFSET + 8;

/// Byte offset of `jobs`.
const JOBS_OFFSET: usize = 8;
/// Byte offset of `recovery`.
const RECOVERY_OFFSET: usize = 10;
/// Byte offset of `cpu_busy_permille`.
const CPU_BUSY_OFFSET: usize = 12;
/// Byte offset of the pressure kind discriminant (`0` = no pressure).
const PRESSURE_KIND_OFFSET: usize = 14;
/// Byte offset of the pressured-resource count (`0` = no pressure).
const PRESSURE_COUNT_OFFSET: usize = 15;
/// Byte offset of the pressure level.
const PRESSURE_LEVEL_OFFSET: usize = 16;
/// Byte offset of the top-task name's length prefix (`0` = no top task).
const TOP_TASK_NAME_LEN_OFFSET: usize = 18;
/// Byte offset of the top-task name text.
const TOP_TASK_NAME_OFFSET: usize = 19;
/// Byte offset of the top-task CPU fraction.
const TOP_TASK_CPU_OFFSET: usize = TOP_TASK_NAME_OFFSET + TRAY_TASK_NAME_MAX;
/// Byte offset of the power-authority flag (`0`/`1`), the last field of the
/// `PublishSummary` block.
const POWER_CAPABLE_OFFSET: usize = TOP_TASK_CPU_OFFSET + 2;

impl SwitchboardRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and the
    /// fixed `PublishSummary` block — the only operation today.
    pub const WIRE_LEN: usize = POWER_CAPABLE_OFFSET + 1;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, SWITCHBOARD_REQUEST_MAGIC);
        put_u16(&mut out, 4, SWITCHBOARD_VERSION_V1);
        match *self {
            Self::PublishSummary { summary } => {
                put_u16(&mut out, 6, OP_PUBLISH_SUMMARY);
                put_u16(&mut out, JOBS_OFFSET, summary.jobs);
                put_u16(&mut out, RECOVERY_OFFSET, summary.recovery);
                put_u16(
                    &mut out,
                    CPU_BUSY_OFFSET,
                    summary.cpu_busy_permille.as_u16(),
                );
                // `None` leaves the pressure/top-task fields zeroed, which
                // is exactly the wire's reserved "absent" encoding.
                if let Some(pressure) = summary.pressure {
                    out[PRESSURE_KIND_OFFSET] = pressure.kind.as_u8();
                    out[PRESSURE_COUNT_OFFSET] = pressure.count.as_u8();
                    put_u16(&mut out, PRESSURE_LEVEL_OFFSET, pressure.level.as_u16());
                }
                if let Some(top_task) = summary.top_task {
                    out[TOP_TASK_NAME_LEN_OFFSET] = top_task.name.len_byte();
                    out[TOP_TASK_NAME_OFFSET..TOP_TASK_NAME_OFFSET + TRAY_TASK_NAME_MAX]
                        .copy_from_slice(top_task.name.raw_bytes());
                    put_u16(
                        &mut out,
                        TOP_TASK_CPU_OFFSET,
                        top_task.cpu_permille.as_u16(),
                    );
                }
                out[POWER_CAPABLE_OFFSET] = u8::from(summary.power_capable);
            }
            Self::ActivateOwner { owner } => {
                put_u16(&mut out, 6, OP_ACTIVATE_OWNER);
                put_u64(&mut out, OWNER_OFFSET, owner);
            }
            Self::RestartOwner { owner } => {
                put_u16(&mut out, 6, OP_RESTART_OWNER);
                put_u64(&mut out, OWNER_OFFSET, owner);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, or a dirty reserved field (a
    ///   non-zero pressured-resource count or pressure level while the
    ///   pressure kind is "none", a non-zero top-task name tail or CPU
    ///   fraction while the name length is "none", or a non-zero summary
    ///   payload on an owner-directed operation).
    /// * [`Errno::AbiVersionUnsupported`] — not `switchboard-v1`.
    /// * [`Errno::OutOfRange`] — an unknown operation, a pressure kind
    ///   outside the closed set, a pressured-resource count of zero or
    ///   above [`TRAY_PRESSURE_KIND_COUNT`] beside a named pressure, a
    ///   permille fraction above `1000`, the reserved zero owner id on
    ///   an owner-directed operation, or the power-authority flag byte
    ///   holding anything but `0`/`1`.
    /// * [`Errno::LengthOutOfRange`] — a top-task name length outside
    ///   `1..=TRAY_TASK_NAME_MAX`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SWITCHBOARD_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SWITCHBOARD_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        match op {
            OP_PUBLISH_SUMMARY => {
                let jobs = read_u16(bytes, JOBS_OFFSET);
                let recovery = read_u16(bytes, RECOVERY_OFFSET);
                let cpu_busy_permille = TrayPermille::new(read_u16(bytes, CPU_BUSY_OFFSET))?;
                let pressure = decode_pressure(bytes)?;
                let top_task = decode_top_task(bytes)?;
                let power_capable = decode_bool(bytes[POWER_CAPABLE_OFFSET])?;
                Ok(Self::PublishSummary {
                    summary: TraySummary {
                        jobs,
                        recovery,
                        cpu_busy_permille,
                        pressure,
                        top_task,
                        power_capable,
                    },
                })
            }
            OP_ACTIVATE_OWNER => Ok(Self::ActivateOwner {
                owner: decode_owner(bytes)?,
            }),
            OP_RESTART_OWNER => Ok(Self::RestartOwner {
                owner: decode_owner(bytes)?,
            }),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Decode a wire boolean flag: exactly `0` or `1`, never a truthy-nonzero
/// convention that would let a malformed frame silently mean something the
/// sender never wrote.
fn decode_bool(byte: u8) -> Result<bool, Errno> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Errno::OutOfRange),
    }
}

/// Decode an owner-directed operation's target: the whole summary payload
/// beyond the task id is reserved on these operations, so a dirty tail
/// refuses rather than being ignored, and the reserved zero task id never
/// names a process.
fn decode_owner(bytes: &[u8]) -> Result<u64, Errno> {
    if bytes[OWNER_TAIL_OFFSET..SwitchboardRequest::WIRE_LEN]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err(Errno::BadMagic);
    }
    let owner = read_u64(bytes, OWNER_OFFSET);
    if owner == 0 {
        return Err(Errno::OutOfRange);
    }
    Ok(owner)
}

/// Decode the pressure block: the reserved "no pressure" kind requires its
/// count and level to be the dirty-reserved zero; any other kind byte is a
/// real [`TrayPressure`], whose count must name at least the dominant
/// resource itself.
fn decode_pressure(bytes: &[u8]) -> Result<Option<TrayPressure>, Errno> {
    let kind_byte = bytes[PRESSURE_KIND_OFFSET];
    let count_byte = bytes[PRESSURE_COUNT_OFFSET];
    let level_raw = read_u16(bytes, PRESSURE_LEVEL_OFFSET);
    if kind_byte == PRESSURE_KIND_NONE {
        if count_byte != 0 || level_raw != 0 {
            return Err(Errno::BadMagic);
        }
        return Ok(None);
    }
    let kind = TrayPressureKind::from_u8(kind_byte)?;
    let level = TrayPermille::new(level_raw)?;
    let count = TrayPressureCount::new(count_byte)?;
    Ok(Some(TrayPressure { kind, level, count }))
}

/// Decode the top-task block: a "no top task" length of zero requires the
/// name buffer and CPU fraction to be the dirty-reserved zero; any other
/// length names a real [`TrayTask`].
fn decode_top_task(bytes: &[u8]) -> Result<Option<TrayTask>, Errno> {
    let name_len = bytes[TOP_TASK_NAME_LEN_OFFSET];
    let mut name_bytes = [0u8; TRAY_TASK_NAME_MAX];
    name_bytes
        .copy_from_slice(&bytes[TOP_TASK_NAME_OFFSET..TOP_TASK_NAME_OFFSET + TRAY_TASK_NAME_MAX]);
    let cpu_raw = read_u16(bytes, TOP_TASK_CPU_OFFSET);
    if name_len == 0 {
        if name_bytes.iter().any(|&b| b != 0) || cpu_raw != 0 {
            return Err(Errno::BadMagic);
        }
        return Ok(None);
    }
    let name = TrayTaskName::from_wire(name_len, &name_bytes)?;
    let cpu_permille = TrayPermille::new(cpu_raw)?;
    Ok(Some(TrayTask { name, cpu_permille }))
}

// --- The publish reply -----------------------------------------------------

/// Reply length, in bytes, of a [`SwitchboardRequest::PublishSummary`]: the
/// shared status word followed by the serving session's [`ProcId`].
pub const SWITCHBOARD_PUBLISH_REPLY_LEN: usize = 4 + PROC_ID_LEN;

/// Encode a successful publish reply, attesting the serving session's own
/// [`ProcId`] to the publisher.
///
/// The service needs the session's identity to authenticate the commands it
/// will later receive on its own mailbox, and the reply is the one place it
/// can learn it without trusting a claim: only the process the kernel let
/// bind the seat-scoped rendezvous can answer a call to it, so the identity
/// in the reply is as good as attested. A refusal is the plain status frame
/// ([`crate::reply::encode_status_reply`]) and carries no identity.
#[must_use]
pub fn encode_publish_reply(session: ProcId) -> [u8; SWITCHBOARD_PUBLISH_REPLY_LEN] {
    let mut out = [0u8; SWITCHBOARD_PUBLISH_REPLY_LEN];
    out[..4].copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    out[4..].copy_from_slice(&session.to_le_bytes());
    out
}

/// Decode a publish reply into the serving session's [`ProcId`].
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * the session's refusal, decoded from the leading status word.
/// * [`Errno::OutOfRange`] — a successful reply naming the kernel sentinel
///   rather than a real process instance (fail closed: an identity that can
///   never authenticate a command is refused at the source rather than
///   stored and silently mismatched later).
/// * [`Errno::LengthOutOfRange`] — a malformed identity.
pub fn decode_publish_reply(bytes: &[u8]) -> Result<ProcId, Errno> {
    if bytes.len() < SWITCHBOARD_PUBLISH_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let session = ProcId::from_bytes(&bytes[4..SWITCHBOARD_PUBLISH_REPLY_LEN])?;
    if session.is_kernel() {
        return Err(Errno::OutOfRange);
    }
    Ok(session)
}

// --- The session -> service command mailbox --------------------------------

/// High tag of a Switchboard instance's command-mailbox endpoint id (see
/// [`command_endpoint_for`]).
const COMMAND_ENDPOINT_TAG: u64 = 0x5747_0000_0000_0000;

/// The command-mailbox endpoint id a Switchboard instance binds for the
/// session's commands: the service's own kernel task id under a fixed high
/// tag, so every instance binds a distinct, collision-free, unreserved id —
/// the same naming rule the window channel's event mailboxes follow, so the
/// two id spaces can never disagree. A pid is bounded to [`crate::PID_MAX`]
/// precisely so it fits beneath the tag.
///
/// The mailbox is owner-only to receive and every message carries its
/// sender's kernel-attested origin, so the id needs no secrecy: the session
/// derives it from the attested origin of the publish call it just answered,
/// never from a wire claim, and the service ignores any message whose
/// attested sender is not the session that answered its publish.
#[must_use]
pub const fn command_endpoint_for(pid: u64) -> u64 {
    COMMAND_ENDPOINT_TAG | (pid & crate::PID_MAX)
}

/// Which panel section a [`SwitchboardCommand::OpenPanel`] opens on.
///
/// Defined here rather than shared with the control library's own section
/// vocabulary because the ABI cannot depend on a userland library; the two
/// are mapped at the service's edge, exactly as the tray pressure kinds are.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CommandSection {
    /// The live-tasks section (the resting section of an ordinary open).
    Tasks = 1,
    /// The resources section: one pane per resource device — the processor,
    /// memory, each volume, each interface, the display path, and the
    /// machine's own identity, seats and authority.
    Resources = 2,
    /// The recovery section (what a long-press on a flagged icon opens).
    Recovery = 3,
}

impl CommandSection {
    /// The wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire discriminant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any byte outside the closed set, including
    /// the reserved `0` (fail closed on a corrupt or hostile frame).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Tasks),
            2 => Ok(Self::Resources),
            3 => Ok(Self::Recovery),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// How many unresponsive owners a single [`SeatReport`] names.
///
/// A validation bound on an untrusted frame, not a capacity: the report is
/// fixed-width, and the honest
/// [`total`](SeatReport::total) tells the panel how many exist beyond the
/// named few, so a machine with more hung apps than this reports truthfully
/// rather than growing the frame.
pub const SEAT_REPORT_OWNERS_MAX: usize = 8;

/// The session's report of which window owners have stopped draining their
/// event mailbox and are therefore unresponsive.
///
/// Only the session can observe this (it is the party whose deliveries are
/// being refused), so it is the one fact the service cannot sample for
/// itself. Owners are named by kernel task id alone: the service already
/// samples the process list, so it joins the ids against names it attested
/// itself rather than trusting display text from the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SeatReport {
    /// How many owners are unresponsive in total, named or not.
    total: u16,
    /// How many entries of `owners` are live.
    count: u8,
    /// The named owners' kernel task ids, `count` of them live.
    owners: [u64; SEAT_REPORT_OWNERS_MAX],
}

impl SeatReport {
    /// Nothing is unresponsive — the resting state of a healthy seat.
    pub const HEALTHY: Self = Self {
        total: 0,
        count: 0,
        owners: [0; SEAT_REPORT_OWNERS_MAX],
    };

    /// Build a report naming `owners` out of `total` unresponsive owners.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — more names than
    ///   [`SEAT_REPORT_OWNERS_MAX`].
    /// * [`Errno::OutOfRange`] — a `total` below the number of names (a
    ///   report cannot name more owners than it counts), the reserved zero
    ///   task id, or the same owner named twice; the type's guarantee is a
    ///   set of live ids, so a contradictory report is refused rather than
    ///   de-duplicated behind the caller's back.
    pub fn new(total: u16, owners: &[u64]) -> Result<Self, Errno> {
        if owners.len() > SEAT_REPORT_OWNERS_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let count = u8::try_from(owners.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if usize::from(total) < owners.len() {
            return Err(Errno::OutOfRange);
        }
        let mut slots = [0u64; SEAT_REPORT_OWNERS_MAX];
        for (slot, &owner) in slots.iter_mut().zip(owners) {
            if owner == 0 {
                return Err(Errno::OutOfRange);
            }
            *slot = owner;
        }
        if slots[..owners.len()]
            .iter()
            .enumerate()
            .any(|(index, owner)| slots[..index].contains(owner))
        {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            total,
            count,
            owners: slots,
        })
    }

    /// How many owners are unresponsive in total, named or not.
    #[must_use]
    pub const fn total(&self) -> u16 {
        self.total
    }

    /// The named owners' kernel task ids.
    #[must_use]
    pub fn owners(&self) -> &[u64] {
        &self.owners[..usize::from(self.count)]
    }
}

/// What one composited desktop frame cost, as the session measured it.
///
/// The session owns the compositor, so it is the only party that can count
/// this; the service samples the kernel, which knows nothing about pixels.
/// Every field is a count of work — never a duration — so the panel shows a
/// reading a reader can act on and a test can assert.
///
/// The headline reading is `damaged_px` against `blended_px` against
/// `screen_px`: a frame that changes three thousand pixels but blends four
/// million of them is paying for depth nobody can see, and one that changes
/// the whole screen to move a cursor is damaging too much.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FrameReport {
    /// The whole screen's pixel count — the denominator of the reading.
    pub screen_px: u64,
    /// Screen pixels the frame recomposed.
    pub damaged_px: u64,
    /// Layer contributions blended to resolve them.
    pub blended_px: u64,
    /// Damaged pixels resolved by copying a fully opaque run instead,
    /// skipping every layer beneath.
    pub opaque_px: u64,
    /// Rectangles the frame recomposed.
    pub dirty_rects: u32,
    /// Calls into the display driver that published the frame.
    pub present_calls: u32,
    /// Window-furniture lookups served from the retained cache.
    pub chrome_hits: u32,
    /// Window-furniture lookups that had to be rendered.
    pub chrome_misses: u32,
}

impl FrameReport {
    /// `true` when the frame recomposed nothing, so the panel says the
    /// desktop is idle rather than showing a row of zeros as if a frame had
    /// been drawn.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.damaged_px == 0 && self.dirty_rects == 0
    }

    /// Refuse a set of counts no compositor pass could have produced.
    ///
    /// The receiver's fail-closed gate, applied where the untrusted frame is
    /// decoded, so the panel never renders a sender's arithmetic. Each rule
    /// holds of every frame the compositor can actually compose:
    ///
    /// * `damaged_px` cannot exceed `screen_px` — the recomposed rectangles
    ///   are clipped to the screen and pairwise disjoint, so their pixels
    ///   sum to at most the screen.
    /// * `dirty_rects` is zero exactly when `damaged_px` is — an empty
    ///   rectangle is never recomposed, so each counted rectangle carries at
    ///   least one pixel.
    /// * `opaque_px` cannot exceed `damaged_px` — a copied opaque run
    ///   resolves damaged pixels, never pixels outside the damage.
    /// * `present_calls` cannot exceed `dirty_rects + 1` — a frame publishes
    ///   at most one driver call per rectangle, and the whole-screen,
    ///   bounding-box, and hardware-layer paths each publish exactly one.
    ///
    /// `blended_px` is deliberately unbounded in both directions: it counts
    /// layer *contributions*, so a stack of windows may blend one damaged
    /// pixel many times, while damage over bare desktop — no window, no
    /// desktop layer, no cursor — resolves to the root fill and blends
    /// nothing at all.
    const fn validate(&self) -> Result<(), Errno> {
        if self.damaged_px > self.screen_px
            || (self.dirty_rects == 0) != (self.damaged_px == 0)
            || self.opaque_px > self.damaged_px
            || self.present_calls > self.dirty_rects.saturating_add(1)
        {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }
}

/// Wire magic of a [`SwitchboardCommand`] frame (`"SWC1"`).
const SWITCHBOARD_COMMAND_MAGIC: u32 = 0x3143_5753;

/// Wire operation discriminant of [`SwitchboardCommand::OpenPanel`].
const OP_OPEN_PANEL: u16 = 1;
/// Wire operation discriminant of [`SwitchboardCommand::SeatReport`].
const OP_SEAT_REPORT: u16 = 2;
/// Wire operation discriminant of [`SwitchboardCommand::Power`].
const OP_POWER: u16 = 3;
/// Wire operation discriminant of [`SwitchboardCommand::FrameReport`].
const OP_FRAME_REPORT: u16 = 4;

/// Byte offset of an [`SwitchboardCommand::OpenPanel`] section.
const SECTION_OFFSET: usize = 8;
/// Byte offset of a report's total unresponsive count.
const REPORT_TOTAL_OFFSET: usize = 10;
/// Byte offset of a report's named-owner count.
const REPORT_COUNT_OFFSET: usize = 12;
/// Byte offset of a report's named-owner ids, aligned so each id sits on
/// its natural boundary within the frame.
const REPORT_OWNERS_OFFSET: usize = 16;
/// Byte offset of a [`SwitchboardCommand::Power`] operation's action code.
/// Shares the same base as [`SECTION_OFFSET`]: each operation reads only its
/// own fields, so the two never observe each other's bytes.
const POWER_ACTION_OFFSET: usize = 8;
/// Byte offset of a frame report's screen pixel count. Each offset below is
/// chained from this one so the counts keep their natural alignment within
/// the frame and the layout cannot drift as fields move.
const FRAME_SCREEN_OFFSET: usize = 8;
/// Byte offset of a frame report's damaged pixel count.
const FRAME_DAMAGED_OFFSET: usize = FRAME_SCREEN_OFFSET + 8;
/// Byte offset of a frame report's blended layer-contribution count.
const FRAME_BLENDED_OFFSET: usize = FRAME_DAMAGED_OFFSET + 8;
/// Byte offset of a frame report's copied opaque-run pixel count.
const FRAME_OPAQUE_OFFSET: usize = FRAME_BLENDED_OFFSET + 8;
/// Byte offset of a frame report's recomposed-rectangle count.
const FRAME_RECTS_OFFSET: usize = FRAME_OPAQUE_OFFSET + 8;
/// Byte offset of a frame report's display-driver present count.
const FRAME_PRESENTS_OFFSET: usize = FRAME_RECTS_OFFSET + 4;
/// Byte offset of a frame report's furniture-cache hit count.
const FRAME_CHROME_HITS_OFFSET: usize = FRAME_PRESENTS_OFFSET + 4;
/// Byte offset of a frame report's furniture-cache miss count.
const FRAME_CHROME_MISSES_OFFSET: usize = FRAME_CHROME_HITS_OFFSET + 4;
/// First reserved byte past a frame report's payload.
const FRAME_END_OFFSET: usize = FRAME_CHROME_MISSES_OFFSET + 4;

/// One command the desktop session sends a Switchboard instance on its
/// per-instance mailbox ([`command_endpoint_for`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardCommand {
    /// Show the overview panel, opened on `section`.
    OpenPanel {
        /// Which section to rest on.
        section: CommandSection,
    },
    /// Hand over the seat's current unresponsive-owner report.
    SeatReport {
        /// The report.
        report: SeatReport,
    },
    /// Hand over what the session's last composited frame cost.
    FrameReport {
        /// The report.
        report: FrameReport,
    },
    /// Perform the machine power transition `action`. Sent only after the
    /// desktop session's own confirmation prompt has been accepted — the
    /// session holds no authority to act itself, so it relays the user's
    /// explicit choice to the one instance that requested
    /// `CAP_SYSTEM_POWER`.
    Power {
        /// Which power transition to perform.
        action: PowerAction,
    },
}

impl SwitchboardCommand {
    /// Encoded size on the wire: magic (4), version (2), op (2), and the
    /// widest operation's fixed payload (the seat report; every other
    /// operation's payload, the frame report's counts included, fits inside
    /// it).
    pub const WIRE_LEN: usize = REPORT_OWNERS_OFFSET + 8 * SEAT_REPORT_OWNERS_MAX;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, SWITCHBOARD_COMMAND_MAGIC);
        put_u16(&mut out, 4, SWITCHBOARD_VERSION_V1);
        match *self {
            Self::OpenPanel { section } => {
                put_u16(&mut out, 6, OP_OPEN_PANEL);
                out[SECTION_OFFSET] = section.as_u8();
            }
            Self::SeatReport { report } => {
                put_u16(&mut out, 6, OP_SEAT_REPORT);
                put_u16(&mut out, REPORT_TOTAL_OFFSET, report.total());
                out[REPORT_COUNT_OFFSET] = report.count;
                for (index, &owner) in report.owners().iter().enumerate() {
                    put_u64(&mut out, REPORT_OWNERS_OFFSET + index * 8, owner);
                }
            }
            Self::FrameReport { report } => {
                put_u16(&mut out, 6, OP_FRAME_REPORT);
                put_u64(&mut out, FRAME_SCREEN_OFFSET, report.screen_px);
                put_u64(&mut out, FRAME_DAMAGED_OFFSET, report.damaged_px);
                put_u64(&mut out, FRAME_BLENDED_OFFSET, report.blended_px);
                put_u64(&mut out, FRAME_OPAQUE_OFFSET, report.opaque_px);
                put_u32(&mut out, FRAME_RECTS_OFFSET, report.dirty_rects);
                put_u32(&mut out, FRAME_PRESENTS_OFFSET, report.present_calls);
                put_u32(&mut out, FRAME_CHROME_HITS_OFFSET, report.chrome_hits);
                put_u32(&mut out, FRAME_CHROME_MISSES_OFFSET, report.chrome_misses);
            }
            Self::Power { action } => {
                put_u16(&mut out, 6, OP_POWER);
                put_u32(&mut out, POWER_ACTION_OFFSET, action.as_u32());
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole command.
    /// * [`Errno::BadMagic`] — wrong magic, or a dirty reserved field (any
    ///   non-zero byte outside the decoded operation's own payload,
    ///   including an owner slot beyond the named count).
    /// * [`Errno::AbiVersionUnsupported`] — not `switchboard-v1`.
    /// * [`Errno::OutOfRange`] — an unknown operation, a section outside
    ///   the closed set, a total below the named count, the reserved zero
    ///   task id, a repeated owner, an unrecognised power-action
    ///   discriminant, or a set of frame counts that contradict each other
    ///   (see [`FrameReport`]).
    /// * [`Errno::LengthOutOfRange`] — a named-owner count above
    ///   [`SEAT_REPORT_OWNERS_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SWITCHBOARD_COMMAND_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SWITCHBOARD_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        match read_u16(bytes, 6) {
            OP_OPEN_PANEL => {
                if bytes[SECTION_OFFSET + 1..Self::WIRE_LEN]
                    .iter()
                    .any(|&byte| byte != 0)
                {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::OpenPanel {
                    section: CommandSection::from_u8(bytes[SECTION_OFFSET])?,
                })
            }
            OP_SEAT_REPORT => Ok(Self::SeatReport {
                report: decode_seat_report(bytes)?,
            }),
            OP_FRAME_REPORT => Ok(Self::FrameReport {
                report: decode_frame_report(bytes)?,
            }),
            OP_POWER => {
                if bytes[POWER_ACTION_OFFSET + 4..Self::WIRE_LEN]
                    .iter()
                    .any(|&byte| byte != 0)
                {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Power {
                    action: PowerAction::from_u32(read_u32(bytes, POWER_ACTION_OFFSET))?,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Decode a seat report, refusing a dirty reserved byte, an over-long
/// named-owner count, and any owner set the report type itself would
/// refuse — the constructor is the single validation, so the wire can never
/// admit a report a caller could not have built.
fn decode_seat_report(bytes: &[u8]) -> Result<SeatReport, Errno> {
    if bytes[SECTION_OFFSET] != 0
        || bytes[SECTION_OFFSET + 1] != 0
        || bytes[REPORT_COUNT_OFFSET + 1..REPORT_OWNERS_OFFSET]
            .iter()
            .any(|&byte| byte != 0)
    {
        return Err(Errno::BadMagic);
    }
    let count = usize::from(bytes[REPORT_COUNT_OFFSET]);
    if count > SEAT_REPORT_OWNERS_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    if bytes[REPORT_OWNERS_OFFSET + count * 8..SwitchboardCommand::WIRE_LEN]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err(Errno::BadMagic);
    }
    let mut owners = [0u64; SEAT_REPORT_OWNERS_MAX];
    for (index, owner) in owners[..count].iter_mut().enumerate() {
        *owner = read_u64(bytes, REPORT_OWNERS_OFFSET + index * 8);
    }
    SeatReport::new(read_u16(bytes, REPORT_TOTAL_OFFSET), &owners[..count])
}

/// Decode a frame report, refusing a dirty reserved byte and any counts the
/// compositor could not have produced.
fn decode_frame_report(bytes: &[u8]) -> Result<FrameReport, Errno> {
    if bytes[FRAME_END_OFFSET..SwitchboardCommand::WIRE_LEN]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err(Errno::BadMagic);
    }
    let report = FrameReport {
        screen_px: read_u64(bytes, FRAME_SCREEN_OFFSET),
        damaged_px: read_u64(bytes, FRAME_DAMAGED_OFFSET),
        blended_px: read_u64(bytes, FRAME_BLENDED_OFFSET),
        opaque_px: read_u64(bytes, FRAME_OPAQUE_OFFSET),
        dirty_rects: read_u32(bytes, FRAME_RECTS_OFFSET),
        present_calls: read_u32(bytes, FRAME_PRESENTS_OFFSET),
        chrome_hits: read_u32(bytes, FRAME_CHROME_HITS_OFFSET),
        chrome_misses: read_u32(bytes, FRAME_CHROME_MISSES_OFFSET),
    };
    report.validate()?;
    Ok(report)
}

#[cfg(test)]
#[path = "switchboard_ipc_tests.rs"]
mod tests;
