//! Structured response records for the `info:` and `stats:` resource
//! namespaces (`plans/ALIAS.md` §14).
//!
//! RustOS has no `/proc` and no `/sys`: the `info:` (mostly stable facts) and
//! `stats:` (time-dependent measurements) resource namespaces are served by
//! the System Information API, never by a virtual file or by text scraping.
//! [`resolve`](crate::resolve::resolve) maps a parsed
//! [`ResourceRef`](rustos_resref::ResourceRef) onto a
//! [`SysinfoQueryId`](rustos_abi::sysinfo::SysinfoQueryId), issues it through
//! the [`Transport`](crate::transport::Transport) seam, and returns one of the
//! typed records defined here — never free-form text.
//!
//! These records are the *userspace* response shape. They are built in the
//! resolving process and consumed in-process (the shell renders them), so they
//! cross no syscall or IPC boundary and carry no wire encoding: the values
//! come from the already-wire-typed `sysinfo-v1` replies, and a wire form is
//! added here only when a boundary (a pipe, `stdinfo`) actually needs one. The
//! [`ResourceResponse::version`] field is the envelope version so that, if such
//! a boundary appears, the shape can be negotiated rather than guessed.
//!
//! The variant sets ([`MetricKind`], [`Unit`], [`ResetBehavior`],
//! [`Producer`], [`ValueKind`], [`Sensitivity`]) are deliberately closed to
//! exactly what a resolver produces today; a new member is added here in place
//! when a producer emits it, never speculatively.

use alloc::string::{String, ToString};

use rustos_abi::time::{Duration64, Time64};
use rustos_abi::{CapabilityId, Errno};

/// Envelope version for the `info:`/`stats:` response records in this build.
pub const RESINFO_VERSION_V1: u16 = 1;

/// The current envelope version produced by this crate.
pub const RESINFO_VERSION_CURRENT: u16 = RESINFO_VERSION_V1;

/// Largest resource reference retained in a [`ResourceResponse::query`], in
/// bytes.
///
/// A served `info:`/`stats:` reference is short; a longer one is rejected at
/// construction rather than truncated (fail closed).
pub const MAX_QUERY_LEN: usize = 128;

/// Largest [`InfoValue`] string, in bytes.
pub const MAX_INFO_VALUE_LEN: usize = 256;

/// Largest [`Metric`] name, in bytes.
pub const MAX_METRIC_NAME_LEN: usize = 64;

/// The service that produced a response.
///
/// Every `info:`/`stats:` value is served by the System Information API, so
/// the only producer today is the `sysinfod` broker.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Producer {
    /// The `/System/Services/sysinfod.app/Run` System Information API broker.
    Sysinfod,
}

/// The authorization under which a response was served.
///
/// Recorded for provenance: a client can see whether a value was public or
/// gated, and by which capability.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Authorization {
    /// The query required no capability (a self- or system-scoped fact that
    /// exposes no principal's secret).
    Unprivileged,
    /// The query was gated on the named capability, which the caller holds.
    Capability(CapabilityId),
}

/// How sensitive an `info:` value is (`plans/ALIAS.md` §6.2, §14.2).
///
/// `info:` values are not assumed public: machine identity, serial numbers,
/// and MAC addresses are marked [`Sensitive`](Self::Sensitive) so a consumer
/// (the shell, a completion card) can treat them with care.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Sensitivity {
    /// Freely displayable (a hostname, an OS version).
    Public,
    /// Identifying or otherwise sensitive (a machine id, a serial number).
    Sensitive,
}

/// The scalar type of an [`InfoValue`] (`plans/ALIAS.md` §14.2).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ValueKind {
    /// A UTF-8 string value.
    Str,
}

/// The kind of a [`Metric`] (`plans/ALIAS.md` §14.3).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MetricKind {
    /// An instantaneous level that rises and falls (free memory).
    Gauge,
    /// A monotonically increasing accumulator (uptime since boot).
    Counter,
}

/// The unit of a [`Metric::value`] (`plans/ALIAS.md` §14.3).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Unit {
    /// A count of bytes.
    Bytes,
    /// A span of whole seconds.
    Seconds,
    /// A dimensionless count of things (open descriptors, live children).
    Count,
    /// A share expressed as whole percentage points (0–100), e.g. the
    /// CPU busy share.
    Percent,
}

/// When a [`Metric`]'s accumulator resets (`plans/ALIAS.md` §14.3).
///
/// A [`MetricKind::Counter`] must state this so a reader can interpret a value
/// that appears to go backwards.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ResetBehavior {
    /// The value never resets over the life of the resource (a gauge).
    Never,
    /// The value resets to zero at each boot (uptime).
    Boot,
}

/// Render a resource-limit soft/hard bound for display, spelling
/// [`RLIMIT_INFINITY`](rustos_abi::RLIMIT_INFINITY) as `unlimited` and any
/// finite bound as its decimal value.
///
/// The one definition of that convention, shared by the `sysinfo` CLI's
/// `limits` table and the `info:limits/*` resolver, so the two can never spell
/// an unlimited bound differently.
#[must_use]
pub fn render_limit_bound(value: u64) -> String {
    if value == rustos_abi::RLIMIT_INFINITY {
        String::from("unlimited")
    } else {
        value.to_string()
    }
}

/// A single `info:` value: one typed, sensitivity-tagged fact
/// (`plans/ALIAS.md` §14.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InfoValue {
    /// The scalar type of [`value`](Self::value).
    pub kind: ValueKind,
    /// How sensitive the value is.
    pub sensitivity: Sensitivity,
    value: String,
}

impl InfoValue {
    /// Construct a string-valued info record.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `value` exceeds [`MAX_INFO_VALUE_LEN`];
    /// the value is never silently truncated.
    pub fn new_str(sensitivity: Sensitivity, value: &str) -> Result<Self, Errno> {
        if value.len() > MAX_INFO_VALUE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            kind: ValueKind::Str,
            sensitivity,
            value: value.to_string(),
        })
    }

    /// The value text.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A single `stats:` metric with the metadata a reader needs to interpret it
/// (`plans/ALIAS.md` §14.3).
///
/// The metric's *source* is the enclosing [`ResourceResponse::query`] (the
/// resource reference it was read from); it is not duplicated here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metric {
    name: String,
    /// Whether the value is a gauge or a counter.
    pub kind: MetricKind,
    /// The unit of [`value`](Self::value).
    pub unit: Unit,
    /// The measurement itself, in [`unit`](Self::unit).
    pub value: u64,
    /// When the measurement was taken.
    pub sample_time: Time64,
    /// The sampling window for a rate, or [`None`] for a gauge or counter
    /// (which have no window). A rate without a window is undefined.
    pub window: Option<Duration64>,
    /// When the value resets.
    pub reset_behavior: ResetBehavior,
}

impl Metric {
    /// Construct a metric.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `name` exceeds [`MAX_METRIC_NAME_LEN`].
    pub fn new(
        name: &str,
        kind: MetricKind,
        unit: Unit,
        value: u64,
        sample_time: Time64,
        window: Option<Duration64>,
        reset_behavior: ResetBehavior,
    ) -> Result<Self, Errno> {
        if name.len() > MAX_METRIC_NAME_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            name: name.to_string(),
            kind,
            unit,
            value,
            sample_time,
            window,
            reset_behavior,
        })
    }

    /// The metric's name (e.g. `mem/used`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The payload of a [`ResourceResponse`]: either a single info value or a
/// single metric.
///
/// Grouped metric responses (`MetricGroup`, `plans/ALIAS.md` §13) are a future
/// variant, added here when a resolver produces one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsePayload {
    /// An `info:` value.
    Info(InfoValue),
    /// A `stats:` metric.
    Metric(Metric),
    /// A `state:` reading: current mutable state (a link state, a bound
    /// address set), rendered like an `info:` value but never a stable
    /// fact — it may change between reads (`plans/ALIAS.md` §6.4).
    State(InfoValue),
}

/// A resolved `info:`/`stats:` response: the shared envelope
/// (`plans/ALIAS.md` §14.1) wrapping one typed payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceResponse {
    /// Envelope version ([`RESINFO_VERSION_CURRENT`] when produced here).
    pub version: u16,
    /// The service that produced the value.
    pub producer: Producer,
    /// The authorization the value was served under.
    pub authorization: Authorization,
    /// When the response was produced.
    pub timestamp: Time64,
    query: String,
    /// The typed payload.
    pub payload: ResponsePayload,
}

impl ResourceResponse {
    /// Assemble a response envelope around `payload`.
    ///
    /// `query` is the resource reference the value answers (rendered from the
    /// parsed reference), retained as the value's source.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `query` exceeds [`MAX_QUERY_LEN`].
    pub fn new(
        producer: Producer,
        authorization: Authorization,
        timestamp: Time64,
        query: &str,
        payload: ResponsePayload,
    ) -> Result<Self, Errno> {
        if query.len() > MAX_QUERY_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            version: RESINFO_VERSION_CURRENT,
            producer,
            authorization,
            timestamp,
            query: query.to_string(),
            payload,
        })
    }

    /// The resource reference this response answers (also the metric source).
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Authorization, InfoValue, Metric, MetricKind, Producer, ResetBehavior, ResourceResponse,
        ResponsePayload, Sensitivity, Unit, ValueKind, MAX_INFO_VALUE_LEN, MAX_METRIC_NAME_LEN,
        MAX_QUERY_LEN, RESINFO_VERSION_CURRENT,
    };
    use rustos_abi::time::Time64;
    use rustos_abi::{CapabilityId, Errno};

    #[test]
    fn info_value_holds_text_and_kind() {
        let v = InfoValue::new_str(Sensitivity::Public, "rustos").expect("value");
        assert_eq!(v.value(), "rustos");
        assert_eq!(v.kind, ValueKind::Str);
        assert_eq!(v.sensitivity, Sensitivity::Public);
    }

    #[test]
    fn info_value_rejects_overlong_text() {
        let big = "x".repeat(MAX_INFO_VALUE_LEN + 1);
        assert_eq!(
            InfoValue::new_str(Sensitivity::Public, &big),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn metric_carries_metadata() {
        let m = Metric::new(
            "mem/used",
            MetricKind::Gauge,
            Unit::Bytes,
            4096,
            Time64::from_secs(10),
            None,
            ResetBehavior::Never,
        )
        .expect("metric");
        assert_eq!(m.name(), "mem/used");
        assert_eq!(m.value, 4096);
        assert_eq!(m.kind, MetricKind::Gauge);
        assert_eq!(m.unit, Unit::Bytes);
        assert_eq!(m.window, None);
        assert_eq!(m.reset_behavior, ResetBehavior::Never);
    }

    #[test]
    fn metric_rejects_overlong_name() {
        let big = "n".repeat(MAX_METRIC_NAME_LEN + 1);
        assert_eq!(
            Metric::new(
                &big,
                MetricKind::Counter,
                Unit::Seconds,
                0,
                Time64::from_secs(0),
                None,
                ResetBehavior::Boot,
            ),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn response_stamps_version_and_keeps_query() {
        let payload = ResponsePayload::Info(
            InfoValue::new_str(Sensitivity::Sensitive, "abcd").expect("value"),
        );
        let r = ResourceResponse::new(
            Producer::Sysinfod,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
            Time64::from_secs(5),
            "info:system/machine-id",
            payload,
        )
        .expect("response");
        assert_eq!(r.version, RESINFO_VERSION_CURRENT);
        assert_eq!(r.query(), "info:system/machine-id");
        assert_eq!(r.producer, Producer::Sysinfod);
        assert_eq!(
            r.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
    }

    #[test]
    fn response_rejects_overlong_query() {
        let big = "s".repeat(MAX_QUERY_LEN + 1);
        let payload =
            ResponsePayload::Info(InfoValue::new_str(Sensitivity::Public, "v").expect("value"));
        assert_eq!(
            ResourceResponse::new(
                Producer::Sysinfod,
                Authorization::Unprivileged,
                Time64::from_secs(0),
                &big,
                payload,
            ),
            Err(Errno::LengthOutOfRange)
        );
    }
}
