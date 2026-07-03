//! Source derivation and stream authority — the log's authority model.
//!
//! A log record has two classes of data: **system-attested** metadata the
//! kernel/journal vouch for, and **caller content** the emitter chose. This
//! module owns the two authority decisions that turn an attested [`Origin`]
//! and a caller's *requests* into the authoritative values a record carries:
//!
//! * [`derive_source`] computes the system-derived [`SourceName`]
//!   (`kernel.<subsystem>`, `user.<uid>.proc.<proc_id>`, …) from the attested
//!   origin. A caller never supplies its own source; a `caller.requested_source`
//!   is only advisory and is screened as a possible spoof by
//!   [`reserved_source_prefix`].
//! * [`resolve_stream`] assigns the effective [`Stream`] from the caller's
//!   requested stream and the origin's trust, downgrading an untrusted request
//!   for a privileged stream to `runtime` and flagging it as a spoof.
//!
//! Everything here is `no_std` and allocation-free: [`SourceName`] is a
//! fixed-capacity inline buffer, so source derivation runs on the kernel's
//! early-boot path with no allocator.
//!
//! # Attestable domains only
//!
//! The full source-derivation order in the log specification names classes —
//! bootstrap driver, supervised service, signed app bundle — that require
//! executable-role metadata the kernel does not yet attest (the [`Origin`]
//! only distinguishes [`TrustDomain::Kernel`] from [`TrustDomain::User`]
//! today). This module derives the cases the kernel *can* vouch for now and
//! falls back to `unknown.*`; the finer classes are added in place here when
//! their producer exists, never invented ahead of an attestation source.

use rustos_abi::{Errno, FieldName, Origin, TrustDomain, PROC_ID_HEX_LEN};

use crate::record::SOURCE_NAME_MAX;
use crate::stream::Stream;

/// Dotted source-name prefixes reserved for system-derived sources.
///
/// A caller's advisory `caller.requested_source` that begins with one of these
/// is a spoofing attempt (it is trying to look like the kernel, a driver, a
/// service, or the audit/security/journal machinery); it is preserved as a
/// caller claim, never allowed to become the authoritative source. `user.*` is
/// deliberately *not* here: it is the system-derived namespace for ordinary
/// principals, but the caller still cannot set it — the source is always
/// derived from the attested origin, never accepted from the caller.
pub const RESERVED_SOURCE_PREFIXES: [&str; 7] = [
    "kernel.",
    "driver.",
    "audit.",
    "security.",
    "journal.",
    "service.",
    "system.",
];

/// The reserved source prefix `name` begins with, or [`None`].
///
/// Screen a caller's requested source through this before it is stored as a
/// claim, so a spoof attempt (`requested_source = "kernel.audit"`) can be
/// recorded as evidence while the trusted [`SourceName`] remains the
/// system-derived value.
#[must_use]
pub fn reserved_source_prefix(name: &str) -> Option<&'static str> {
    RESERVED_SOURCE_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
}

/// A system-derived source name, stored inline (no allocation).
///
/// Source names are dotted grouping labels (`kernel.mem`, `service.<id>`,
/// `user.1000.proc.<hex>`) bounded by [`SOURCE_NAME_MAX`]. The value is always
/// produced by [`derive_source`] from an attested [`Origin`]; there is no
/// constructor that accepts a caller-supplied string, so an authoritative
/// source can only ever be a derived one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SourceName {
    buf: [u8; SOURCE_NAME_MAX],
    len: usize,
}

impl SourceName {
    /// The derived source as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Every byte was written from validated ASCII (`user.`/`kernel.`
        // literals, decimal digits, lowercase-hex, or a grammar-checked
        // subsystem), so the range is valid UTF-8; fall back to the empty
        // string rather than panic if that invariant were ever violated.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// The fallback for an early kernel record before its subsystem is known
    /// (or when a kernel subsystem label is malformed): `unknown.kernel`.
    #[must_use]
    fn unknown_kernel() -> Self {
        // The literal fits `SOURCE_NAME_MAX`, so this cannot fail.
        let mut b = Builder::new();
        let _ = b.push_str("unknown.kernel");
        b.finish()
    }
}

/// Derive the authoritative [`SourceName`] from an attested origin.
///
/// * [`TrustDomain::Kernel`] with a valid `subsystem` label →
///   `kernel.<subsystem>`; with no (or a malformed) subsystem →
///   `unknown.kernel`.
/// * [`TrustDomain::User`] → `user.<uid>.proc.<proc_id_hex>`.
///
/// `subsystem`, when present, must obey the [`FieldName`] grammar
/// (`[a-z][a-z0-9_]{0,63}`); it is trusted kernel-supplied input, but a
/// malformed label fails closed to `unknown.kernel` rather than emitting a
/// malformed or namespace-escaping source. This never panics and never
/// allocates.
#[must_use]
pub fn derive_source(origin: &Origin, subsystem: Option<&str>) -> SourceName {
    match origin.trust_domain() {
        TrustDomain::Kernel => kernel_source(subsystem),
        TrustDomain::User => user_source(origin).unwrap_or_else(|_| SourceName::unknown_kernel()),
    }
}

fn kernel_source(subsystem: Option<&str>) -> SourceName {
    match subsystem {
        Some(name) if FieldName::new(name).is_ok() => {
            let mut b = Builder::new();
            if b.push_str("kernel.").is_err() || b.push_str(name).is_err() {
                return SourceName::unknown_kernel();
            }
            b.finish()
        }
        _ => SourceName::unknown_kernel(),
    }
}

fn user_source(origin: &Origin) -> Result<SourceName, Errno> {
    let mut b = Builder::new();
    b.push_str("user.")?;
    b.push_u32(origin.uid())?;
    b.push_str(".proc.")?;
    let mut hex = [0u8; PROC_ID_HEX_LEN];
    b.push_str(origin.proc_id().write_hex(&mut hex))?;
    Ok(b.finish())
}

/// A fixed-capacity, fail-closed builder for a [`SourceName`].
struct Builder {
    buf: [u8; SOURCE_NAME_MAX],
    len: usize,
}

impl Builder {
    const fn new() -> Self {
        Self {
            buf: [0u8; SOURCE_NAME_MAX],
            len: 0,
        }
    }

    fn push_bytes(&mut self, src: &[u8]) -> Result<(), Errno> {
        let end = self
            .len
            .checked_add(src.len())
            .ok_or(Errno::BufferTooSmall)?;
        let dst = self
            .buf
            .get_mut(self.len..end)
            .ok_or(Errno::BufferTooSmall)?;
        dst.copy_from_slice(src);
        self.len = end;
        Ok(())
    }

    fn push_str(&mut self, s: &str) -> Result<(), Errno> {
        self.push_bytes(s.as_bytes())
    }

    fn push_u32(&mut self, mut v: u32) -> Result<(), Errno> {
        // u32::MAX is 10 decimal digits.
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push_bytes(&digits[i..])
    }

    fn finish(self) -> SourceName {
        SourceName {
            buf: self.buf,
            len: self.len,
        }
    }
}

/// The journal's decision about which stream a record is actually placed on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StreamDecision {
    /// The stream the record is committed to.
    pub effective: Stream,
    /// `true` when the caller requested a privileged stream it was not trusted
    /// for; the request was downgraded to [`Stream::Runtime`] and should be
    /// preserved as a `caller.requested_stream` claim (and may itself warrant a
    /// trusted security record).
    pub spoofed: bool,
}

/// Resolve the effective stream for a record from its attested origin and the
/// caller's requested stream.
///
/// A [`TrustDomain::Kernel`] principal is trusted for every stream, so its
/// request is honoured. A [`TrustDomain::User`] principal may only write the
/// caller-writable streams (`runtime`, `debug`); a request for a
/// trusted-emitter stream (`boot`/`security`/`audit`/`journal`) is denied,
/// downgraded to [`Stream::Runtime`], and flagged as a spoof. An absent request
/// defaults to `runtime`.
///
/// Finer trust distinctions (a supervised system/security service that may
/// legitimately write `security`/`audit`) require a trust domain the kernel
/// does not yet attest; this resolver grows in place when that domain exists.
#[must_use]
pub fn resolve_stream(origin: &Origin, requested: Option<Stream>) -> StreamDecision {
    let requested = requested.unwrap_or(Stream::Runtime);
    let trusted = matches!(origin.trust_domain(), TrustDomain::Kernel);
    if trusted || !requested.requires_trusted_emitter() {
        StreamDecision {
            effective: requested,
            spoofed: false,
        }
    } else {
        StreamDecision {
            effective: Stream::Runtime,
            spoofed: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_source, reserved_source_prefix, resolve_stream, SourceName};
    use crate::stream::Stream;
    use rustos_abi::{
        CapabilitySummary, Origin, ProcId, TrustDomain, ORIGIN_CONSOLE_NONE, PROC_ID_LEN,
    };

    fn kernel_origin() -> Origin {
        Origin::new(
            TrustDomain::Kernel,
            0,
            0,
            1,
            ProcId::KERNEL,
            CapabilitySummary::EMPTY,
            ORIGIN_CONSOLE_NONE,
        )
    }

    fn user_origin(uid: u32, proc_id: [u8; PROC_ID_LEN]) -> Origin {
        Origin::new(
            TrustDomain::User,
            uid,
            uid,
            42,
            ProcId::from_raw(proc_id),
            CapabilitySummary::EMPTY,
            ORIGIN_CONSOLE_NONE,
        )
    }

    #[test]
    fn kernel_subsystem_derives_dotted_source() {
        let s = derive_source(&kernel_origin(), Some("mem"));
        assert_eq!(s.as_str(), "kernel.mem");
    }

    #[test]
    fn kernel_without_subsystem_falls_back_to_unknown() {
        let s = derive_source(&kernel_origin(), None);
        assert_eq!(s.as_str(), "unknown.kernel");
    }

    #[test]
    fn kernel_with_malformed_subsystem_fails_closed() {
        // A subsystem carrying a `.` would otherwise let a kernel emitter
        // synthesise `kernel.audit.x`; the grammar rejects it, so it falls
        // back rather than escaping the namespace.
        for bad in ["Net", "net.audit", "", "a b", "n\n"] {
            let s = derive_source(&kernel_origin(), Some(bad));
            assert_eq!(s.as_str(), "unknown.kernel", "bad subsystem {bad:?}");
        }
    }

    #[test]
    fn user_source_is_uid_and_proc_id() {
        let s = derive_source(&user_origin(1000, [0xAB; PROC_ID_LEN]), None);
        assert_eq!(
            s.as_str(),
            "user.1000.proc.abababababababababababababababab"
        );
    }

    #[test]
    fn user_source_ignores_any_subsystem_hint() {
        // The subsystem label is a kernel-context concept; a user record must
        // never fold it into the source.
        let s = derive_source(&user_origin(0, [0u8; PROC_ID_LEN]), Some("mem"));
        assert_eq!(s.as_str(), "user.0.proc.00000000000000000000000000000000");
    }

    #[test]
    fn user_source_formats_max_uid() {
        let s = derive_source(&user_origin(u32::MAX, [0x01; PROC_ID_LEN]), None);
        assert!(s.as_str().starts_with("user.4294967295.proc."));
        assert!(s.as_str().len() <= super::SOURCE_NAME_MAX);
    }

    #[test]
    fn reserved_prefixes_are_detected() {
        assert_eq!(reserved_source_prefix("kernel.audit"), Some("kernel."));
        assert_eq!(reserved_source_prefix("driver.net.eth0"), Some("driver."));
        assert_eq!(reserved_source_prefix("security.mac"), Some("security."));
        assert_eq!(reserved_source_prefix("service.devmgr"), Some("service."));
        assert_eq!(reserved_source_prefix("system.time"), Some("system."));
    }

    #[test]
    fn non_reserved_names_pass_screening() {
        assert_eq!(reserved_source_prefix("dhcp"), None);
        assert_eq!(reserved_source_prefix("my.component"), None);
        // A leading `user.` is derived-only but not a *reserved* spoof prefix;
        // the caller still cannot set the authoritative source.
        assert_eq!(reserved_source_prefix("user.1000.proc.x"), None);
    }

    #[test]
    fn kernel_may_write_any_stream() {
        for want in [
            Stream::Boot,
            Stream::Runtime,
            Stream::Debug,
            Stream::Security,
            Stream::Audit,
            Stream::Journal,
        ] {
            let d = resolve_stream(&kernel_origin(), Some(want));
            assert_eq!(d.effective, want);
            assert!(!d.spoofed);
        }
    }

    #[test]
    fn user_may_write_runtime_and_debug() {
        let o = user_origin(1000, [1u8; PROC_ID_LEN]);
        for want in [Stream::Runtime, Stream::Debug] {
            let d = resolve_stream(&o, Some(want));
            assert_eq!(d.effective, want);
            assert!(!d.spoofed);
        }
    }

    #[test]
    fn user_privileged_stream_request_is_downgraded_and_flagged() {
        let o = user_origin(1000, [1u8; PROC_ID_LEN]);
        for want in [
            Stream::Boot,
            Stream::Security,
            Stream::Audit,
            Stream::Journal,
        ] {
            let d = resolve_stream(&o, Some(want));
            assert_eq!(d.effective, Stream::Runtime, "want {want:?}");
            assert!(d.spoofed, "want {want:?}");
        }
    }

    #[test]
    fn absent_request_defaults_to_runtime() {
        assert_eq!(
            resolve_stream(&user_origin(1, [1u8; PROC_ID_LEN]), None),
            super::StreamDecision {
                effective: Stream::Runtime,
                spoofed: false,
            }
        );
        // The kernel default is also runtime when it requests nothing.
        assert_eq!(
            resolve_stream(&kernel_origin(), None).effective,
            Stream::Runtime
        );
    }

    #[test]
    fn unknown_kernel_is_a_valid_bounded_source() {
        let s = SourceName::unknown_kernel();
        assert_eq!(s.as_str(), "unknown.kernel");
    }
}
