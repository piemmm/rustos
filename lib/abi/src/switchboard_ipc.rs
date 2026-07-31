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
//! The single operation is the fixed-width [`SwitchboardRequest`], answered
//! with the shared status frame ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]) — success, or a typed refusal.
//! Every decode fails closed: an unknown magic, version, or operation, an
//! out-of-range pressure kind, pressured-resource count, or permille
//! fraction, an over-long or malformed top-task name, or a dirty reserved
//! field refuses rather than guessing.

use crate::bounded_text::BoundedText;
use crate::le::{put_u16, put_u32, read_u16, read_u32};
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
}

/// One Switchboard tray-summary channel operation
/// (`plans/NEW-TASKBAR.md` T9/T10).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardRequest {
    /// Publish (replacing any prior) the caller's tray-signal summary.
    PublishSummary {
        /// The compact summary to show on the tray icon.
        summary: TraySummary,
    },
}

/// Wire operation discriminant of [`SwitchboardRequest::PublishSummary`].
const OP_PUBLISH_SUMMARY: u16 = 1;

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

impl SwitchboardRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and the
    /// fixed `PublishSummary` block — the only operation today.
    pub const WIRE_LEN: usize = TOP_TASK_CPU_OFFSET + 2;

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
    ///   pressure kind is "none", or a non-zero top-task name tail or CPU
    ///   fraction while the name length is "none").
    /// * [`Errno::AbiVersionUnsupported`] — not `switchboard-v1`.
    /// * [`Errno::OutOfRange`] — an unknown operation, a pressure kind
    ///   outside the closed set, a pressured-resource count of zero or
    ///   above [`TRAY_PRESSURE_KIND_COUNT`] beside a named pressure, or a
    ///   permille fraction above `1000`.
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
                Ok(Self::PublishSummary {
                    summary: TraySummary {
                        jobs,
                        recovery,
                        cpu_busy_permille,
                        pressure,
                        top_task,
                    },
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
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

#[cfg(test)]
#[path = "switchboard_ipc_tests.rs"]
mod tests;
