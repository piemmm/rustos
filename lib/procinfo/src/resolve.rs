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
//! already exist.

use alloc::string::{String, ToString};

use rustos_abi::sysinfo::{KernelMemoryStats, SysinfoQueryId, SystemIdentity, Uptime};
use rustos_abi::time::Time64;
use rustos_abi::{CapabilityId, Errno};
use rustos_resref::{KnownNamespace, ResourceRef};

use crate::list::field_lossy;
use crate::request::{call, CallError};
use crate::resinfo::{
    Authorization, InfoValue, Metric, MetricKind, Producer, ResetBehavior, ResourceResponse,
    ResponsePayload, Sensitivity, Unit,
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
    let identity = query_identity(transport)?;
    let value = match selector(reference).as_slice() {
        ["system", "hostname"] => {
            InfoValue::new_str(Sensitivity::Public, &field_lossy(identity.hostname_bytes()))
        }
        ["system", "kernel"] => InfoValue::new_str(
            Sensitivity::Public,
            &version_string(&identity),
        ),
        // Machine identity is identifying, not public (`plans/ALIAS.md` §6.2).
        ["system", "machine-id"] => {
            InfoValue::new_str(Sensitivity::Sensitive, &hex_lower(&identity.machine_id))
        }
        _ => return Err(ResolveInfoError::UnknownSelector),
    }
    .map_err(|_| ResolveInfoError::Malformed)?;
    envelope(reference, now, Authorization::Unprivileged, ResponsePayload::Info(value))
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
        ["mem", leaf @ ("used" | "available" | "total")] => {
            let stats = query_kernel_memory(transport)?;
            // The or-pattern above fixes `leaf` to one of these three, so the
            // final arm is `total` and there is no unhandled case.
            let value = if *leaf == "used" {
                stats.total_bytes.saturating_sub(stats.free_bytes)
            } else if *leaf == "available" {
                stats.free_bytes
            } else {
                stats.total_bytes
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
    use rustos_abi::sysinfo::{
        KernelMemoryStats, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::{CapabilityId, Errno};
    use rustos_resref::parse;

    /// An in-memory `sysinfod` stand-in that answers the three singleton
    /// queries this resolver uses, decoding the request exactly as the real
    /// service and optionally denying a chosen query.
    struct Fixture {
        identity: SystemIdentity,
        uptime: Uptime,
        memory: KernelMemoryStats,
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
                deny: None,
                seen: RefCell::new(Vec::new()),
            }
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
                _ => Err(Errno::NotFound),
            }
        }
    }

    fn now() -> Time64 {
        Time64::from_secs(5200)
    }

    fn resolve_str(s: &str, fixture: &Fixture) -> Result<super::ResourceResponse, ResolveInfoError> {
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
