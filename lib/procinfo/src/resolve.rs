//! The userspace `info:`/`stats:` resource resolver (`plans/ALIAS.md` §6.2,
//! §6.3, §14).
//!
//! `info:` and `stats:` references are **not** kernel-owned resources: they
//! name facts and measurements that must be served by the System Information
//! API, never by a virtual file, by text scraping, or by a kernel
//! `resource_open` backing (which would bypass the `sysinfod` broker's
//! per-principal scoping). This resolver is therefore userspace: it maps a
//! parsed [`ResourceRef`] onto a [`SysinfoQueryId`], issues it through the
//! shared [`Transport`] seam (the same path `ps`/`sysinfo` use), and turns
//! the typed reply into a [`ResourceResponse`] (`crate::resinfo`).
//!
//! It fails closed: an unknown selector, a decoration (`@guard`, `::facet`,
//! `?param`) on a resource that takes none, a capability the caller does not
//! hold, or a reply that does not decode all yield a typed
//! [`ResolveInfoError`] and never a fabricated value (`AGENTS.md` §5.4, §2.9).
//! The served set grows in place here as sibling queries land; today it covers
//! exactly the ungated/self-scoped and kernel-memory `sysinfo-v1` queries that
//! already exist (`info:system/{hostname,kernel,machine-id,boot-time}`,
//! `info:process/{pid,uid,gid,proc-id}`, `info:limits/<kind>/{soft,hard}`,
//! `stats:uptime`, `stats:mem/*`, and `stats:limits/<kind>`).

use alloc::string::{String, ToString};

use rustos_abi::origin::Origin;
use rustos_abi::sysinfo::{
    KernelMemoryStats, ResourceLimitRecord, SysinfoQueryId, SystemIdentity, Uptime,
    RESOURCE_LIMITS_REPORT_LEN,
};
use rustos_abi::time::Time64;
use rustos_abi::{CapabilityId, Errno, LimitKind, ResourceLimit};
use rustos_resref::{KnownNamespace, ResourceRef};

use crate::list::field_lossy;
use crate::request::{call, CallError};
use crate::resinfo::{
    render_limit_bound, Authorization, InfoValue, Metric, MetricKind, Producer, ResetBehavior,
    ResourceResponse, ResponsePayload, Sensitivity, Unit,
};
use crate::transport::Transport;

/// Why resolving an `info:`/`stats:` reference did not produce a value.
///
/// A resolver-level error, distinct from the parser's syntax errors: the
/// reference parsed but names nothing this resolver serves, requests a shape
/// it does not offer, or could not be answered by the service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveInfoError {
    /// The reference is not in the `info:` or `stats:` namespace; this
    /// resolver does not own it (the caller routed it to the wrong resolver).
    NotInfoOrStats,
    /// The namespace is served but the selector names no resource in it.
    UnknownSelector,
    /// The selector is understood but the request is not serviceable: a
    /// guard, facet, or query parameter on a resource that takes none.
    UnsupportedRequest,
    /// The System Information API refused the query for want of the
    /// capability it declares.
    CapabilityDenied,
    /// The System Information API call failed for another reason.
    Service(Errno),
    /// The service's reply did not decode as the expected record.
    Malformed,
}

/// Resolve an `info:`/`stats:` `reference` to a [`ResourceResponse`], reading
/// the value from the System Information API through `transport` and stamping
/// the envelope with `now`.
///
/// # Errors
///
/// A [`ResolveInfoError`] naming the first refusal; no value is produced on
/// any error path (fail closed).
pub fn resolve(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    match reference.namespace().known() {
        Some(KnownNamespace::Info) => resolve_info(reference, now, transport),
        Some(KnownNamespace::Stats) => resolve_stats(reference, now, transport),
        _ => Err(ResolveInfoError::NotInfoOrStats),
    }
}

/// Resolve an `info:` reference (a stable fact) to a single [`InfoValue`].
fn resolve_info(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    reject_decoration(reference)?;
    let value = resolve_info_value(&selector(reference), transport)?;
    envelope(
        reference,
        now,
        Authorization::Unprivileged,
        ResponsePayload::Info(value),
    )
}

/// Map an `info:` `selector` onto its typed value, issuing only the System
/// Information query the matched selector actually needs.
fn resolve_info_value(
    selector: &[&str],
    transport: &dyn Transport,
) -> Result<InfoValue, ResolveInfoError> {
    let value = match selector {
        ["system", "hostname"] => InfoValue::new_str(
            Sensitivity::Public,
            &field_lossy(query_identity(transport)?.hostname_bytes()),
        ),
        ["system", "kernel"] => InfoValue::new_str(
            Sensitivity::Public,
            &version_string(&query_identity(transport)?),
        ),
        // Machine identity is identifying, not public (`plans/ALIAS.md` §6.2).
        ["system", "machine-id"] => InfoValue::new_str(
            Sensitivity::Sensitive,
            &hex_lower(&query_identity(transport)?.machine_id),
        ),
        // The wall-clock instant of boot is fixed for the life of the boot, so
        // it is a stable fact rather than a measurement; it is not sensitive.
        // It rides the same ungated `UPTIME` query that `stats:uptime` uses.
        ["system", "boot-time"] => InfoValue::new_str(
            Sensitivity::Public,
            &time_string(query_uptime(transport)?.boot_time),
        ),
        // The caller's own kernel-attested identity. The self-scoped
        // `PROCESS_IDENTITY` query needs no capability and answers only for the
        // asking principal, so these are public facts about the caller itself,
        // not a cross-principal disclosure.
        ["process", leaf @ ("pid" | "uid" | "gid" | "proc-id")] => {
            let origin = query_process_identity(transport)?;
            // The or-pattern fixes `leaf` to one of these four, so the final
            // arm is `proc-id` and there is no unhandled case.
            match *leaf {
                "pid" => InfoValue::new_str(Sensitivity::Public, &origin.pid().to_string()),
                "uid" => InfoValue::new_str(Sensitivity::Public, &origin.uid().to_string()),
                "gid" => InfoValue::new_str(Sensitivity::Public, &origin.gid().to_string()),
                _ => {
                    InfoValue::new_str(Sensitivity::Public, &hex_lower(origin.proc_id().as_bytes()))
                }
            }
        }
        // A configured soft/hard bound on one of the caller's own resources.
        // The self-scoped `RESOURCE_LIMITS` query needs no capability and
        // answers only for the asking principal, so its own limits are public
        // facts about itself, not a cross-principal disclosure. An unlimited
        // bound renders as `unlimited`, sharing the one spelling the `limits`
        // CLI uses.
        ["limits", kind_name, bound @ ("soft" | "hard")] => {
            let kind = LimitKind::from_name(kind_name).ok_or(ResolveInfoError::UnknownSelector)?;
            let limit = limit_for(kind, transport)?;
            // The or-pattern fixes `bound` to `soft` or `hard`, so the final
            // arm is `hard` and there is no unhandled case.
            let rendered = match *bound {
                "soft" => render_limit_bound(limit.soft),
                _ => render_limit_bound(limit.hard),
            };
            InfoValue::new_str(Sensitivity::Public, &rendered)
        }
        _ => return Err(ResolveInfoError::UnknownSelector),
    };
    value.map_err(|_| ResolveInfoError::Malformed)
}

/// Resolve a `stats:` reference (a measurement) to a single [`Metric`].
fn resolve_stats(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    reject_decoration(reference)?;
    match selector(reference).as_slice() {
        ["uptime"] => {
            let uptime = query_uptime(transport)?;
            // A monotonic span since boot never precedes boot; clamp the
            // signed span to a non-negative count of seconds.
            let secs = u64::try_from(uptime.since_boot.secs().max(0)).unwrap_or(0);
            let metric = Metric::new(
                "uptime",
                MetricKind::Counter,
                Unit::Seconds,
                secs,
                now,
                None,
                ResetBehavior::Boot,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                Authorization::Unprivileged,
                ResponsePayload::Metric(metric),
            )
        }
        ["mem", leaf @ ("used" | "available" | "total" | "kernel-heap" | "user-resident")] => {
            let stats = query_kernel_memory(transport)?;
            // The or-pattern above fixes `leaf` to one of these five, so the
            // final arm is `user-resident` and there is no unhandled case.
            let value = match *leaf {
                "used" => stats.total_bytes.saturating_sub(stats.free_bytes),
                "available" => stats.free_bytes,
                "total" => stats.total_bytes,
                "kernel-heap" => stats.kernel_heap_bytes,
                _ => stats.user_resident_bytes,
            };
            let mut name = String::from("mem/");
            name.push_str(leaf);
            let metric = Metric::new(
                &name,
                MetricKind::Gauge,
                Unit::Bytes,
                value,
                now,
                None,
                ResetBehavior::Never,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                // The kernel-memory query is gated on `CAP_SYSINFO_KERNEL`;
                // the broker enforces it, and a denial surfaces below.
                Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
                ResponsePayload::Metric(metric),
            )
        }
        // The caller's own live usage of one of its limited resources: a
        // measurement, so a gauge (it rises and falls and never resets over
        // the life of the process). Byte-denominated resources report
        // [`Unit::Bytes`]; the rest are a dimensionless [`Unit::Count`]. The
        // query is ungated and self-scoped, so the usage is unprivileged.
        ["limits", kind_name] => {
            let kind = LimitKind::from_name(kind_name).ok_or(ResolveInfoError::UnknownSelector)?;
            let usage = usage_for(kind, transport)?;
            let mut name = String::from("limits/");
            name.push_str(kind_name);
            let metric = Metric::new(
                &name,
                MetricKind::Gauge,
                unit_for_limit(kind),
                usage,
                now,
                None,
                ResetBehavior::Never,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                Authorization::Unprivileged,
                ResponsePayload::Metric(metric),
            )
        }
        _ => Err(ResolveInfoError::UnknownSelector),
    }
}

/// Refuse a guard, facet, or query parameter: none of the resources this
/// resolver serves takes one, so a decorated reference is not serviceable.
fn reject_decoration(reference: &ResourceRef) -> Result<(), ResolveInfoError> {
    if reference.guard().is_some() || reference.facet().is_some() || !reference.params().is_empty()
    {
        return Err(ResolveInfoError::UnsupportedRequest);
    }
    Ok(())
}

/// Borrow the reference's selector segments as string slices for matching.
fn selector(reference: &ResourceRef) -> alloc::vec::Vec<&str> {
    reference.selector().iter().map(String::as_str).collect()
}

/// Wrap `payload` in the shared response envelope.
fn envelope(
    reference: &ResourceRef,
    now: Time64,
    authorization: Authorization,
    payload: ResponsePayload,
) -> Result<ResourceResponse, ResolveInfoError> {
    ResourceResponse::new(
        Producer::Sysinfod,
        authorization,
        now,
        &reference.to_string(),
        payload,
    )
    .map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::SYSTEM_IDENTITY`] and decode the reply.
fn query_identity(transport: &dyn Transport) -> Result<SystemIdentity, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::SYSTEM_IDENTITY, &[]).map_err(map_call_error)?;
    SystemIdentity::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::UPTIME`] and decode the reply.
fn query_uptime(transport: &dyn Transport) -> Result<Uptime, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::UPTIME, &[]).map_err(map_call_error)?;
    Uptime::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::KERNEL_MEMORY_STATS`] and decode the reply.
fn query_kernel_memory(transport: &dyn Transport) -> Result<KernelMemoryStats, ResolveInfoError> {
    let reply =
        call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[]).map_err(map_call_error)?;
    KernelMemoryStats::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::PROCESS_IDENTITY`] and decode the caller's own
/// kernel-attested [`Origin`].
fn query_process_identity(transport: &dyn Transport) -> Result<Origin, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::PROCESS_IDENTITY, &[]).map_err(map_call_error)?;
    Origin::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::RESOURCE_LIMITS`] and decode the caller's own
/// per-resource limits, indexed by [`LimitKind`] discriminant.
///
/// The reply is exactly [`LimitKind::COUNT`] [`ResourceLimitRecord`]s packed
/// in discriminant order. A reply of any other length, a record that does not
/// decode, or a record whose self-describing `kind` disagrees with its
/// position is corrupt and fails closed as [`ResolveInfoError::Malformed`] —
/// never a partial or mis-attributed answer.
fn query_resource_limits(
    transport: &dyn Transport,
) -> Result<[ResourceLimitRecord; LimitKind::COUNT], ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::RESOURCE_LIMITS, &[]).map_err(map_call_error)?;
    if reply.len() != RESOURCE_LIMITS_REPORT_LEN {
        return Err(ResolveInfoError::Malformed);
    }
    let mut records =
        [ResourceLimitRecord::new(LimitKind::AddressSpaceBytes, ResourceLimit::UNLIMITED, 0);
            LimitKind::COUNT];
    for (index, kind) in LimitKind::ALL.iter().enumerate() {
        let base = index * ResourceLimitRecord::WIRE_LEN;
        let record =
            ResourceLimitRecord::from_bytes(&reply[base..base + ResourceLimitRecord::WIRE_LEN])
                .map_err(|_| ResolveInfoError::Malformed)?;
        // Records are positional in discriminant order; the self-describing
        // kind field must agree with the slot it occupies, or the reply is
        // corrupt.
        if record.kind != *kind {
            return Err(ResolveInfoError::Malformed);
        }
        records[index] = record;
    }
    Ok(records)
}

/// The caller's effective soft/hard bound for `kind`.
fn limit_for(
    kind: LimitKind,
    transport: &dyn Transport,
) -> Result<ResourceLimit, ResolveInfoError> {
    let records = query_resource_limits(transport)?;
    Ok(records[kind.as_u32() as usize].limit)
}

/// The caller's current live usage of `kind`, in its natural unit.
fn usage_for(kind: LimitKind, transport: &dyn Transport) -> Result<u64, ResolveInfoError> {
    let records = query_resource_limits(transport)?;
    Ok(records[kind.as_u32() as usize].usage)
}

/// The unit a resource's live usage is measured in: bytes for the
/// byte-denominated resources, a dimensionless count for the rest.
fn unit_for_limit(kind: LimitKind) -> Unit {
    match kind {
        LimitKind::AddressSpaceBytes | LimitKind::StackBytes => Unit::Bytes,
        LimitKind::OpenStreams | LimitKind::Processes => Unit::Count,
    }
}

/// Map a transport [`CallError`] onto the resolver's error vocabulary.
fn map_call_error(err: CallError) -> ResolveInfoError {
    match err {
        CallError::PermissionDenied => ResolveInfoError::CapabilityDenied,
        CallError::Service(errno) => ResolveInfoError::Service(errno),
    }
}

/// The OS version as `major.minor.patch`.
fn version_string(identity: &SystemIdentity) -> String {
    let mut s = String::new();
    push_u16(&mut s, identity.version_major);
    s.push('.');
    push_u16(&mut s, identity.version_minor);
    s.push('.');
    push_u16(&mut s, identity.version_patch);
    s
}

/// Append the decimal spelling of `value` to `out`.
fn push_u16(out: &mut String, value: u16) {
    // A `u16` is at most five decimal digits; format without `alloc::fmt`
    // machinery so the helper stays trivially bounded.
    let mut buf = [0u8; 5];
    let mut n = value;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &b in &buf[i..] {
        out.push(b as char);
    }
}

/// The decimal, epoch-relative spelling of an instant, losslessly: whole
/// seconds, and a nine-digit zero-padded fraction only when the sub-second
/// field is non-zero (e.g. `1719936000` or `1719936000.000000040`).
fn time_string(instant: Time64) -> String {
    let mut s = instant.secs().to_string();
    let nanos = instant.subsec_nanos();
    if nanos != 0 {
        s.push('.');
        let digits = nanos.to_string();
        // A canonical sub-second field is `< NANOS_PER_SEC`, so `digits` is at
        // most nine characters; left-pad the shorter cases to keep the place
        // value of each digit.
        for _ in 0..(9 - digits.len()) {
            s.push('0');
        }
        s.push_str(&digits);
    }
    s
}

/// Lowercase-hex encoding of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{resolve, ResolveInfoError};
    use crate::resinfo::{
        Authorization, MetricKind, Producer, ResetBehavior, ResponsePayload, Sensitivity, Unit,
    };
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::origin::{CapabilitySummary, Origin, ProcId, TrustDomain};
    use rustos_abi::sysinfo::{
        KernelMemoryStats, ResourceLimitRecord, SysinfoQueryId, SysinfoRequestHeader,
        SystemIdentity, Uptime,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::{CapabilityId, Errno, LimitKind, ResourceLimit};
    use rustos_resref::parse;

    /// An in-memory `sysinfod` stand-in that answers the singleton queries
    /// this resolver uses, decoding the request exactly as the real service
    /// and optionally denying a chosen query.
    struct Fixture {
        identity: SystemIdentity,
        uptime: Uptime,
        memory: KernelMemoryStats,
        origin: Origin,
        limits: [ResourceLimitRecord; LimitKind::COUNT],
        deny: Option<SysinfoQueryId>,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                identity: SystemIdentity::new([0xAB; 16], 1, 2, 3, b"rustbox").expect("identity"),
                uptime: Uptime {
                    since_boot: Duration64::from_secs(4200),
                    boot_time: Time64::from_secs(1000),
                },
                memory: KernelMemoryStats {
                    total_bytes: 8192,
                    free_bytes: 2048,
                    kernel_heap_bytes: 512,
                    user_resident_bytes: 4096,
                    page_size: 4096,
                    reserved: 0,
                },
                origin: Origin::new(
                    TrustDomain::User,
                    1000,
                    50,
                    42,
                    ProcId::from_raw([0xCD; 16]),
                    CapabilitySummary::EMPTY,
                ),
                // One record per `LimitKind`, in discriminant order. `Processes`
                // is left unlimited so the `unlimited` rendering is exercised.
                limits: [
                    ResourceLimitRecord::new(
                        LimitKind::AddressSpaceBytes,
                        ResourceLimit::new(1_048_576, 2_097_152).expect("well-formed"),
                        4096,
                    ),
                    ResourceLimitRecord::new(
                        LimitKind::OpenStreams,
                        ResourceLimit::new(16, 32).expect("well-formed"),
                        5,
                    ),
                    ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 3),
                    ResourceLimitRecord::new(
                        LimitKind::StackBytes,
                        ResourceLimit::new(8192, 8192).expect("well-formed"),
                        2048,
                    ),
                ],
                deny: None,
                seen: RefCell::new(Vec::new()),
            }
        }

        /// The `RESOURCE_LIMITS` reply: the four records packed in
        /// discriminant order, exactly as the real service frames it.
        fn limits_report(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for record in &self.limits {
                out.extend_from_slice(&record.to_le_bytes());
            }
            out
        }
    }

    impl crate::transport::Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            match header.query {
                SysinfoQueryId::SYSTEM_IDENTITY => Ok(self.identity.to_le_bytes().to_vec()),
                SysinfoQueryId::UPTIME => Ok(self.uptime.to_le_bytes().to_vec()),
                SysinfoQueryId::KERNEL_MEMORY_STATS => Ok(self.memory.to_le_bytes().to_vec()),
                SysinfoQueryId::PROCESS_IDENTITY => Ok(self.origin.to_le_bytes().to_vec()),
                SysinfoQueryId::RESOURCE_LIMITS => Ok(self.limits_report()),
                _ => Err(Errno::NotFound),
            }
        }
    }

    fn now() -> Time64 {
        Time64::from_secs(5200)
    }

    fn resolve_str(
        s: &str,
        fixture: &Fixture,
    ) -> Result<super::ResourceResponse, ResolveInfoError> {
        let reference = parse(s).expect("parse");
        resolve(&reference, now(), fixture)
    }

    #[test]
    fn info_hostname_is_public_text() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/hostname", &fixture).expect("ok");
        assert_eq!(r.producer, Producer::Sysinfod);
        assert_eq!(r.authorization, Authorization::Unprivileged);
        assert_eq!(r.query(), "info:system/hostname");
        match r.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "rustbox");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_kernel_version_is_dotted() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/kernel", &fixture).expect("ok");
        match r.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "1.2.3"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_machine_id_is_sensitive_hex() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/machine-id", &fixture).expect("ok");
        match r.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "abababababababababababababababab");
                assert_eq!(v.sensitivity, Sensitivity::Sensitive);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_boot_time_is_public_epoch_seconds() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/boot-time", &fixture).expect("ok");
        assert_eq!(r.authorization, Authorization::Unprivileged);
        assert_eq!(r.query(), "info:system/boot-time");
        match r.payload {
            ResponsePayload::Info(v) => {
                // The fixture's boot instant is 1000 s with a zero sub-second
                // field, so the fraction is omitted.
                assert_eq!(v.value(), "1000");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_process_identity_fields_are_public_and_self_scoped() {
        let fixture = Fixture::new();
        for (selector, expected) in [
            ("info:process/pid", "42"),
            ("info:process/uid", "1000"),
            ("info:process/gid", "50"),
            ("info:process/proc-id", "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"),
        ] {
            let r = resolve_str(selector, &fixture).expect("ok");
            assert_eq!(r.authorization, Authorization::Unprivileged);
            assert_eq!(r.query(), selector);
            match r.payload {
                ResponsePayload::Info(v) => {
                    assert_eq!(v.value(), expected);
                    assert_eq!(v.sensitivity, Sensitivity::Public);
                }
                ResponsePayload::Metric(_) => panic!("expected info value"),
            }
        }
        // Every field rode the one self-scoped, ungated identity query.
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .all(|q| *q == SysinfoQueryId::PROCESS_IDENTITY));
    }

    #[test]
    fn info_process_unknown_leaf_fails_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:process/parent", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn info_process_malformed_reply_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("info:process/pid").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn time_string_pads_and_omits_the_sub_second_fraction() {
        // A zero sub-second field prints no fraction.
        assert_eq!(super::time_string(Time64::from_secs(1000)), "1000");
        // A non-zero sub-second field is nine-digit zero-padded, losslessly.
        assert_eq!(
            super::time_string(Time64::new(1_719_936_000, 40).expect("instant")),
            "1719936000.000000040"
        );
        assert_eq!(
            super::time_string(Time64::new(0, 999_999_999).expect("instant")),
            "0.999999999"
        );
        // Instants before the epoch keep their sign.
        assert_eq!(super::time_string(Time64::from_secs(-5)), "-5");
    }

    #[test]
    fn stats_uptime_is_a_boot_counter_in_seconds() {
        let fixture = Fixture::new();
        let r = resolve_str("stats:uptime", &fixture).expect("ok");
        assert_eq!(r.authorization, Authorization::Unprivileged);
        match r.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "uptime");
                assert_eq!(m.value, 4200);
                assert_eq!(m.kind, MetricKind::Counter);
                assert_eq!(m.unit, Unit::Seconds);
                assert_eq!(m.reset_behavior, ResetBehavior::Boot);
                assert_eq!(m.window, None);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn stats_mem_used_and_available_are_gated_gauges() {
        let fixture = Fixture::new();
        let used = resolve_str("stats:mem/used", &fixture).expect("ok");
        assert_eq!(
            used.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        match used.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/used");
                assert_eq!(m.value, 8192 - 2048);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let avail = resolve_str("stats:mem/available", &fixture).expect("ok");
        match avail.payload {
            ResponsePayload::Metric(m) => assert_eq!(m.value, 2048),
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let total = resolve_str("stats:mem/total", &fixture).expect("ok");
        match total.payload {
            ResponsePayload::Metric(m) => assert_eq!(m.value, 8192),
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn stats_mem_kernel_heap_and_user_resident_are_gated_gauges() {
        let fixture = Fixture::new();
        let heap = resolve_str("stats:mem/kernel-heap", &fixture).expect("ok");
        assert_eq!(
            heap.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        match heap.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/kernel-heap");
                assert_eq!(m.value, 512);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let resident = resolve_str("stats:mem/user-resident", &fixture).expect("ok");
        match resident.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/user-resident");
                assert_eq!(m.value, 4096);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn stats_limits_usage_are_unprivileged_gauges() {
        let fixture = Fixture::new();
        // A byte-denominated resource reports its usage in bytes.
        let addr = resolve_str("stats:limits/address-space-bytes", &fixture).expect("ok");
        assert_eq!(addr.authorization, Authorization::Unprivileged);
        match addr.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "limits/address-space-bytes");
                assert_eq!(m.value, 4096);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
                assert_eq!(m.window, None);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        // A countable resource reports a dimensionless count.
        let procs = resolve_str("stats:limits/processes", &fixture).expect("ok");
        match procs.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "limits/processes");
                assert_eq!(m.value, 3);
                assert_eq!(m.unit, Unit::Count);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn info_limits_bounds_are_public_facts() {
        let fixture = Fixture::new();
        let soft = resolve_str("info:limits/open-streams/soft", &fixture).expect("ok");
        assert_eq!(soft.authorization, Authorization::Unprivileged);
        assert_eq!(soft.query(), "info:limits/open-streams/soft");
        match soft.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "16");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
        let hard = resolve_str("info:limits/open-streams/hard", &fixture).expect("ok");
        match hard.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "32"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
        // An unlimited bound renders as `unlimited`, not as a raw sentinel.
        let unlimited = resolve_str("info:limits/processes/soft", &fixture).expect("ok");
        match unlimited.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "unlimited"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn limits_unknown_kind_or_bound_fails_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("stats:limits/nope", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("info:limits/nope/soft", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        // A known kind but an unknown bound word matches no arm.
        assert_eq!(
            resolve_str("info:limits/processes/median", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn limits_reply_wrong_length_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("stats:limits/processes").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn limits_reply_kind_out_of_order_fails_closed() {
        // A full-length report whose records are packed in the wrong order:
        // the self-describing `kind` no longer matches its slot, so the reply
        // is rejected rather than mis-attributed.
        struct Scrambled;
        impl crate::transport::Transport for Scrambled {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                let mut out = Vec::new();
                // `OpenStreams` sits where `AddressSpaceBytes` (slot 0) belongs.
                for kind in [
                    LimitKind::OpenStreams,
                    LimitKind::AddressSpaceBytes,
                    LimitKind::Processes,
                    LimitKind::StackBytes,
                ] {
                    out.extend_from_slice(
                        &ResourceLimitRecord::new(kind, ResourceLimit::UNLIMITED, 0).to_le_bytes(),
                    );
                }
                Ok(out)
            }
        }
        let reference = parse("info:limits/open-streams/soft").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Scrambled),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn denied_kernel_memory_maps_to_capability_denied() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        assert_eq!(
            resolve_str("stats:mem/used", &fixture),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn unknown_selectors_fail_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:system/nope", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:mem/pagefaults", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:cpu/load", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn decorations_are_unserviceable() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:system/hostname::record", &fixture),
            Err(ResolveInfoError::UnsupportedRequest)
        );
        assert_eq!(
            resolve_str("stats:uptime?window=1s", &fixture),
            Err(ResolveInfoError::UnsupportedRequest)
        );
    }

    #[test]
    fn wrong_namespace_is_not_ours() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("sys:random", &fixture),
            Err(ResolveInfoError::NotInfoOrStats)
        );
    }

    #[test]
    fn malformed_reply_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("info:system/hostname").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }
}
