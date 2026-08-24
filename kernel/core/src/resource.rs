//! Resource-reference resolution for [`resource_open`](crate::syscalls).
//!
//! TAIRiX has no `/dev`, `/proc`, or `/sys`: a typed *non-filesystem*
//! resource (a random source, a null sink, a device endpoint) is named by a
//! resource reference (`plans/ALIAS.md`), e.g. `sys:random`, not by a
//! pseudo-file. The single shared reference parser [`tairix_resref`] turns
//! the caller's string into a typed [`ResourceRef`] (spelling only — it never
//! resolves, opens, or capability-checks). This module is the kernel-side
//! *resolver*: it maps a parsed reference to a concrete [`ResourceBacking`]
//! the descriptor layer serves, checking the caller's authority per
//! namespace and failing closed on anything it does not recognise or serve.
//!
//! **This resolver serves only kernel-owned backings.** The `info:`,
//! `state:`, and `stats:` namespaces are deliberately *not* served here: they are the
//! System Information API's facts and measurements, which must flow through
//! the `sysinfod` broker so its per-principal scoping is applied. Resolving
//! them in the kernel would bypass that broker — the forbidden bypass the
//! charter and `plans/ALIAS.md` name — so they are resolved in userspace
//! (`lib/procinfo`) over the sysinfo query API, and this resolver fails them
//! closed like any other non-kernel namespace.
//!
//! Only the `sys:` namespace's unprivileged members (`sys:random`,
//! `sys:null`) are served today, and every other namespace fails closed
//! rather than fabricating a resource — but the two reasons a namespace can
//! fail are kept apart, because they mean different things to the caller:
//!
//! * A **stream-backed** namespace with no resolver wired yet (`disk:`,
//!   `tty:`, …) is [`ResolveError::UnsupportedResolver`] →
//!   [`Errno::NotImplemented`]. It could be served here later; a caller may
//!   reasonably expect this reference to open on a future build.
//! * A **value-backed** namespace ([`NamespaceBacking::Value`] — `info:`,
//!   `state:`, `stats:`) is [`ResolveError::NotAStream`] →
//!   [`Errno::NotSupported`]. It will *never* be served here: a typed value
//!   read through a broker is not a kernel byte stream, so no build of this
//!   resolver will open one and the caller is told so rather than being left
//!   to wait for a resolver that is never coming. Such a value is read
//!   *above* the kernel — by `sysinfo show`, by a tool resolving its own
//!   operand, or by the shell's read redirection, each through the broker
//!   under the caller's own identity — which is why nothing is lost by
//!   refusing it here.
//!
//! Both are fail-closed refusals; the distinction is diagnostic, and it is
//! made from the registry's own [`KnownNamespace::backing`] so the kernel and
//! the shell can never disagree about which namespaces are streams. A
//! kernel-owned namespace (a future device endpoint) gains its resolver in
//! place as its consumer appears — the resolver contract here does not
//! change.

use tairix_abi::{Errno, OpenFlags};
use tairix_resref::{parse, KnownNamespace, NamespaceBacking, RefError, ResourceRef};

/// The concrete kernel-side backing a resolved resource reference names.
///
/// A descriptor opened by `resource_open` carries one of these instead of a
/// filesystem path; the `stream`-style read/write path dispatches on it. The
/// set grows in place as namespaces gain resolvers (`AGENTS.md` §2.13).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ResourceBacking {
    /// `sys:null` — the discard sink and empty source: a read yields no bytes
    /// (immediate end of stream), a write accepts and discards every byte.
    Null,
    /// `sys:random` — the kernel CSPRNG output (the same reserve
    /// [`random_get`](crate::syscalls) draws from). Read-only; the entropy
    /// pool is never a caller-writable byte sink.
    Random,
}

impl ResourceBacking {
    /// Whether the backing can be opened for reading.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        match self {
            // `sys:null` reads as an empty stream; `sys:random` yields bytes.
            Self::Null | Self::Random => true,
        }
    }

    /// Whether the backing can be opened for writing.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        match self {
            Self::Null => true,
            // The randomness subsystem is never a caller-writable sink; entropy
            // injection is a separate privileged, typed operation.
            Self::Random => false,
        }
    }
}

/// Why resolving a resource reference to a backing failed.
///
/// A resolver-level error, distinct from the parser's syntax errors: the
/// reference was well-formed (or not) but names nothing this kernel serves,
/// or requests access the backing does not offer. [`ResolveError::to_errno`]
/// maps it to the stable user/kernel [`Errno`] for the syscall boundary; the
/// precise variant is retained for the audit record.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ResolveError {
    /// The string is not a well-formed resource reference (a parser refusal,
    /// or a bare filesystem path with no `namespace:` prefix).
    InvalidSyntax,
    /// The namespace is not one this specification defines.
    UnknownNamespace,
    /// The namespace is known but no resolver is wired for it yet (it has no
    /// consumer): fail closed rather than fabricate a resource. The namespace
    /// *is* a byte stream ([`NamespaceBacking::Stream`]), so a later build may
    /// serve it.
    UnsupportedResolver,
    /// The namespace is value-backed ([`NamespaceBacking::Value`]): its
    /// members are typed values read through the System Information API
    /// broker, not byte streams, so no descriptor can ever be opened on one.
    ///
    /// Distinct from [`UnsupportedResolver`](Self::UnsupportedResolver): that
    /// says "not yet", this says "not ever". Retrying, or waiting for a
    /// future build, can never turn this into a success — the caller must
    /// read the value through a broker client instead
    /// (`sysinfo show <reference>`, or the shell's read redirection
    /// `cmd < <reference>`, which resolves the value in userspace and hands
    /// the child a pipe).
    NotAStream,
    /// The selector names no resource within its (served) namespace.
    UnknownSelector,
    /// The reference is understood but the request is not serviceable — an
    /// unsupported guard/facet/query on a resource that has none, or an
    /// access direction the backing does not offer (e.g. writing
    /// `sys:random`), or an open with no direction at all.
    UnsupportedRequest,
}

impl ResolveError {
    /// Map to the stable user/kernel [`Errno`].
    ///
    /// A malformed or unserviceable request collapses onto
    /// [`Errno::OutOfRange`] (the closest `abi-v1` "invalid request" code); an
    /// unknown namespace or selector onto [`Errno::NotFound`] (the resource
    /// does not exist); a known-but-unwired stream namespace onto
    /// [`Errno::NotImplemented`] (no resolver serves it *yet*); and a
    /// value-backed namespace onto [`Errno::NotSupported`], whose contract is
    /// exactly this case — the subsystem is live, this backing cannot
    /// represent the request, and retrying can never succeed. The precise
    /// [`ResolveError`] is retained in-kernel for the audit log.
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::InvalidSyntax | Self::UnsupportedRequest => Errno::OutOfRange,
            Self::UnknownNamespace | Self::UnknownSelector => Errno::NotFound,
            Self::UnsupportedResolver => Errno::NotImplemented,
            Self::NotAStream => Errno::NotSupported,
        }
    }

    /// A stable, lowercase identifier for the audit log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSyntax => "invalid_syntax",
            Self::UnknownNamespace => "unknown_namespace",
            Self::UnsupportedResolver => "unsupported_resolver",
            Self::NotAStream => "not_a_stream",
            Self::UnknownSelector => "unknown_selector",
            Self::UnsupportedRequest => "unsupported_request",
        }
    }
}

/// Resolve `reference` to a [`ResourceBacking`], validating the requested
/// `flags` against what the backing offers.
///
/// Parses the reference with the single shared parser, refuses a namespace
/// the registry marks value-backed, dispatches on the namespace, and — on
/// success — confirms the open direction is one the backing supports. Fails
/// closed with a typed [`ResolveError`] on any malformed, unknown, unwired,
/// non-stream, or unserviceable reference; it never returns a backing the
/// caller did not ask for or may not use.
///
/// # Errors
///
/// The [`ResolveError`] naming the first refusal.
pub fn resolve(reference: &str, flags: OpenFlags) -> Result<ResourceBacking, ResolveError> {
    let parsed = parse(reference).map_err(map_ref_error)?;
    let namespace = parsed
        .namespace()
        .known()
        .ok_or(ResolveError::UnknownNamespace)?;
    // A value-backed namespace is refused on its shape, before any
    // selector work: `info:`/`state:`/`stats:` are typed values read through
    // the System Information API broker, so no selector within them could
    // ever be a byte stream this resolver serves. Saying so — rather than
    // "no resolver yet" — is the difference between a refusal the caller can
    // act on and one that invites a pointless retry.
    if namespace.backing() == NamespaceBacking::Value {
        return Err(ResolveError::NotAStream);
    }
    let backing = match namespace {
        KnownNamespace::Sys => resolve_sys(&parsed)?,
        // Every other stream namespace has no kernel resolver wired yet: fail
        // closed rather than pretend the resource exists. A kernel-owned
        // namespace gains its resolver in place when its consumer lands.
        _ => return Err(ResolveError::UnsupportedResolver),
    };
    validate_access(backing, flags)?;
    Ok(backing)
}

/// Resolve a `sys:` reference to its backing.
///
/// `sys:` names abstract kernel-backed resources. The two served today are
/// `sys:null` and `sys:random`; a bare selector with no guard/facet/query.
fn resolve_sys(parsed: &ResourceRef) -> Result<ResourceBacking, ResolveError> {
    // These resources take no identity guard, facet, or query parameter; a
    // reference carrying one is not serviceable (fail closed).
    if parsed.guard().is_some() || parsed.facet().is_some() || !parsed.params().is_empty() {
        return Err(ResolveError::UnsupportedRequest);
    }
    match parsed.selector() {
        [only] if only.as_str() == "null" => Ok(ResourceBacking::Null),
        [only] if only.as_str() == "random" => Ok(ResourceBacking::Random),
        _ => Err(ResolveError::UnknownSelector),
    }
}

/// Confirm `flags` requests only access `backing` offers.
///
/// A resource descriptor is a sequential byte stream. Following the POSIX
/// open(2) treatment of device files, the file-disposition flags a writing
/// tool routinely passes are tolerated as no-ops: `CREATE` creates nothing
/// (the resource already exists once it resolves), and `TRUNCATE` / `APPEND`
/// are positionless on a stream — so `tee sys:null` works exactly as
/// `tee /dev/null` does. `DIRECTORY` is refused (a resource is not a
/// directory) and so is `EXCLUSIVE` (the exclusive-creation request can
/// never be satisfied by an object that already exists). The open must
/// request at least one of read/write, and each requested direction must be
/// one the backing offers.
fn validate_access(backing: ResourceBacking, flags: OpenFlags) -> Result<(), ResolveError> {
    let unserviceable = OpenFlags::DIRECTORY.union(OpenFlags::EXCLUSIVE);
    if flags.bits() & unserviceable.bits() != 0 {
        return Err(ResolveError::UnsupportedRequest);
    }
    if !flags.is_read() && !flags.is_write() {
        return Err(ResolveError::UnsupportedRequest);
    }
    if flags.is_read() && !backing.is_readable() {
        return Err(ResolveError::UnsupportedRequest);
    }
    if flags.is_write() && !backing.is_writable() {
        return Err(ResolveError::UnsupportedRequest);
    }
    Ok(())
}

/// Map a parser refusal to the resolver's error vocabulary.
///
/// Every syntactic refusal — including a bare filesystem path with no
/// `namespace:` prefix ([`RefError::NotAReference`]) — is an invalid
/// resource reference at this boundary.
const fn map_ref_error(_err: RefError) -> ResolveError {
    ResolveError::InvalidSyntax
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sys_random_resolves_read_only() {
        assert_eq!(
            resolve("sys:random", OpenFlags::READ),
            Ok(ResourceBacking::Random)
        );
    }

    #[test]
    fn sys_random_rejects_write() {
        assert_eq!(
            resolve("sys:random", OpenFlags::READ.union(OpenFlags::WRITE)),
            Err(ResolveError::UnsupportedRequest)
        );
        assert_eq!(
            resolve("sys:random", OpenFlags::WRITE),
            Err(ResolveError::UnsupportedRequest)
        );
    }

    #[test]
    fn sys_null_resolves_read_and_write() {
        assert_eq!(
            resolve("sys:null", OpenFlags::READ),
            Ok(ResourceBacking::Null)
        );
        assert_eq!(
            resolve("sys:null", OpenFlags::WRITE),
            Ok(ResourceBacking::Null)
        );
        assert_eq!(
            resolve("sys:null", OpenFlags::READ.union(OpenFlags::WRITE)),
            Ok(ResourceBacking::Null)
        );
    }

    #[test]
    fn open_with_no_direction_fails_closed() {
        assert_eq!(
            resolve("sys:null", OpenFlags::empty()),
            Err(ResolveError::UnsupportedRequest)
        );
    }

    /// A writing tool opens its output the way `tee` does — `WRITE` plus
    /// `CREATE` and a `TRUNCATE`/`APPEND` disposition. On a resource those
    /// dispositions are no-ops (POSIX device semantics), so `tee sys:null`
    /// resolves exactly as `tee /dev/null` opens.
    #[test]
    fn file_disposition_flags_are_tolerated() {
        for disposition in [OpenFlags::TRUNCATE, OpenFlags::APPEND] {
            assert_eq!(
                resolve(
                    "sys:null",
                    OpenFlags::WRITE.union(OpenFlags::CREATE).union(disposition)
                ),
                Ok(ResourceBacking::Null)
            );
        }
    }

    #[test]
    fn directory_and_exclusive_are_refused() {
        assert_eq!(
            resolve("sys:random", OpenFlags::READ.union(OpenFlags::DIRECTORY)),
            Err(ResolveError::UnsupportedRequest)
        );
        assert_eq!(
            resolve(
                "sys:null",
                OpenFlags::WRITE
                    .union(OpenFlags::CREATE)
                    .union(OpenFlags::EXCLUSIVE)
            ),
            Err(ResolveError::UnsupportedRequest)
        );
    }

    /// The registry cross-check: every concrete selector `lib/resref`
    /// catalogues for a namespace *this* resolver serves must actually
    /// resolve for reading, so completion and documentation built on the
    /// registry can never advertise a name the kernel refuses.
    ///
    /// A catalogued namespace resolved in userspace instead (`info:`,
    /// `state:`, `stats:` — served by `lib/procinfo` over the System
    /// Information API) is value-backed, so it is expected to land on
    /// [`ResolveError::NotAStream`] here; its own cross-check lives with that
    /// resolver. A stream namespace awaiting a resolver lands on
    /// [`ResolveError::UnsupportedResolver`]. Entries carrying a placeholder
    /// name a shape rather than one resource, so there is nothing concrete to
    /// open.
    #[test]
    fn catalogued_selectors_resolve() {
        use alloc::format;

        for ns in KnownNamespace::ALL {
            for entry in ns.selector_catalogue() {
                if entry.has_placeholder() {
                    continue;
                }
                let reference = format!("{}:{}", ns.as_str(), entry.selector);
                // A catalogued reference this resolver does not own is
                // refused for exactly the reason its backing implies;
                // anything else is a registry error.
                let expected = match ns.backing() {
                    NamespaceBacking::Value => ResolveError::NotAStream,
                    NamespaceBacking::Stream => ResolveError::UnsupportedResolver,
                };
                if let Err(err) = resolve(&reference, OpenFlags::READ) {
                    assert_eq!(
                        err, expected,
                        "{reference} is catalogued but this resolver refuses it differently",
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_sys_selector_is_not_found() {
        assert_eq!(
            resolve("sys:nope", OpenFlags::READ),
            Err(ResolveError::UnknownSelector)
        );
    }

    #[test]
    fn guard_facet_or_query_on_sys_is_unserviceable() {
        assert_eq!(
            resolve("sys:random@7K2M", OpenFlags::READ),
            Err(ResolveError::UnsupportedRequest)
        );
        assert_eq!(
            resolve("sys:random::raw", OpenFlags::READ),
            Err(ResolveError::UnsupportedRequest)
        );
        assert_eq!(
            resolve("sys:random?window=1s", OpenFlags::READ),
            Err(ResolveError::UnsupportedRequest)
        );
    }

    /// A stream namespace with no resolver wired yet fails closed as
    /// "not implemented": it could be served on a later build.
    #[test]
    fn unwired_stream_namespace_fails_closed() {
        for reference in ["disk:backup", "tty:debug", "gpu:0"] {
            assert_eq!(
                resolve(reference, OpenFlags::READ),
                Err(ResolveError::UnsupportedResolver),
                "{reference} is a stream awaiting its resolver"
            );
            assert_eq!(
                ResolveError::UnsupportedResolver.to_errno(),
                Errno::NotImplemented
            );
        }
    }

    /// A value-backed namespace is refused as "not a stream", not as "not
    /// implemented": `info:`/`state:`/`stats:` are typed values read through
    /// the System Information API broker, so *this* resolver can never serve
    /// one and says so (`Errno::NotSupported`, 32) rather than implying a
    /// resolver is merely missing (`NotImplemented`, 12).
    ///
    /// The refusal is unconditional and stays that way. That userspace readers
    /// serve these references over the broker changes nothing here: routing
    /// them through the kernel would bypass the broker's per-principal
    /// scoping, which is the whole reason this arm exists.
    #[test]
    fn a_value_backed_namespace_is_not_a_stream() {
        for reference in [
            "info:mem/physical",
            "info:system/hostname",
            "state:net/resolver/servers",
            "stats:cpu/load",
        ] {
            let err = resolve(reference, OpenFlags::READ);
            assert_eq!(
                err,
                Err(ResolveError::NotAStream),
                "{reference} is value-backed"
            );
            assert_eq!(
                err.unwrap_err().to_errno(),
                Errno::NotSupported,
                "{reference} reports the honest errno"
            );
        }
        // The refusal is on the namespace's shape, so it does not depend on
        // the selector naming anything real, nor on the open direction.
        assert_eq!(
            resolve("info:nonsuch/leaf", OpenFlags::READ),
            Err(ResolveError::NotAStream)
        );
        assert_eq!(
            resolve("info:mem/physical", OpenFlags::WRITE),
            Err(ResolveError::NotAStream)
        );
        // And the audit record carries the precise reason, not the errno.
        assert_eq!(ResolveError::NotAStream.as_str(), "not_a_stream");
    }

    #[test]
    fn unknown_namespace_is_not_found() {
        assert_eq!(
            resolve("bogus:thing", OpenFlags::READ),
            Err(ResolveError::UnknownNamespace)
        );
    }

    #[test]
    fn a_bare_path_is_not_a_reference() {
        assert_eq!(
            resolve("/System/Kernel", OpenFlags::READ),
            Err(ResolveError::InvalidSyntax)
        );
    }

    #[test]
    fn errno_mapping_is_stable() {
        assert_eq!(ResolveError::InvalidSyntax.to_errno(), Errno::OutOfRange);
        assert_eq!(
            ResolveError::UnsupportedRequest.to_errno(),
            Errno::OutOfRange
        );
        assert_eq!(ResolveError::UnknownNamespace.to_errno(), Errno::NotFound);
        assert_eq!(ResolveError::UnknownSelector.to_errno(), Errno::NotFound);
        assert_eq!(
            ResolveError::UnsupportedResolver.to_errno(),
            Errno::NotImplemented
        );
        // "not ever" is a different errno from "not yet": a caller can tell a
        // backing that cannot represent the request from one whose resolver
        // has not landed.
        assert_eq!(ResolveError::NotAStream.to_errno(), Errno::NotSupported);
        assert_ne!(
            ResolveError::NotAStream.to_errno(),
            ResolveError::UnsupportedResolver.to_errno()
        );
    }

    #[test]
    fn resource_ref_max_matches_the_parser_bound() {
        // The ABI wire bound and the parser's own maximum must not drift: a
        // reference the ABI accepts always fits the parser.
        assert_eq!(tairix_abi::RESOURCE_REF_MAX, tairix_resref::MAX_REF_LEN);
    }
}
