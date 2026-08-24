//! Reading a value-backed resource reference as bytes.
//!
//! `info:`, `state:`, and `stats:` are typed values served by the `sysinfod`
//! broker, so the kernel resource resolver opens no descriptor on one:
//! serving them kernel-side would bypass the broker's per-principal scoping.
//! This is the read that goes *through* the broker instead — the resolver's
//! value, rendered as the bytes a reader consumes.
//!
//! It confers no authority. The query is the one the frozen registry gates,
//! so a caller lacking the declared capability is refused here exactly as
//! `sysinfo show` refuses it. There is no write direction: a value-backed
//! resource is changed by a typed service command.

use alloc::string::String;

use tairix_resref::{KnownNamespace, NamespaceBacking, ResourceRef};

use tairix_abi::time::Time64;

use crate::resinfo::MAX_INFO_VALUE_LEN;
use crate::resolve::{resolve, ResolveInfoError};
use crate::transport::Transport;

/// Largest byte length [`read_value`] can produce, newline included.
///
/// Derived from the payload bounds, not chosen: an
/// [`InfoValue`](crate::resinfo::InfoValue) is bounded at construction and a
/// [`Metric`](crate::resinfo::Metric) renders as a decimal `u64`. Stated so a
/// caller can size a pipe write against it — it is far below one pipe's ring
/// (`kernel/core/src/pipe.rs::PIPE_CAPACITY`), so such a write never blocks
/// for want of a reader.
pub const MAX_VALUE_LEN: usize = MAX_INFO_VALUE_LEN + 1;

/// Read the value-backed `reference` as the bare value plus one newline.
///
/// The rendering is the shared
/// [`display_value`](crate::resinfo::ResourceResponse::display_value), so a
/// value read here and one printed by `sysinfo show` cannot disagree. The
/// newline makes the stream line-shaped, so a text tool's line loop sees a
/// complete line rather than an unterminated fragment.
///
/// `now` stamps the envelope; the caller supplies it so a test needs no clock.
///
/// # Errors
///
/// * [`ResolveInfoError::NamespaceNotServed`] if `reference` is not
///   value-backed — a stream reference is the kernel resolver's, and there is
///   one reader per backing.
/// * Whatever [`resolve`] refuses with, notably
///   [`ResolveInfoError::CapabilityDenied`] naming the query whose capability
///   the caller lacks.
/// * [`ResolveInfoError::Malformed`] if the render exceeds [`MAX_VALUE_LEN`];
///   it is never truncated to fit.
pub fn read_value(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<String, ResolveInfoError> {
    // The shared registry classifies the backing; this path holds no list of
    // its own, so it cannot drift from what the kernel resolver refuses.
    if reference.namespace().known().map(KnownNamespace::backing) != Some(NamespaceBacking::Value) {
        return Err(ResolveInfoError::NamespaceNotServed);
    }
    let response = resolve(reference, now, transport)?;
    let mut rendered = response.display_value();
    rendered.push('\n');
    if rendered.len() > MAX_VALUE_LEN {
        return Err(ResolveInfoError::Malformed);
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use tairix_abi::sysinfo::{
        KernelMemoryStats, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
    };
    use tairix_abi::time::{Duration64, Time64};
    use tairix_abi::{CapabilityId, Errno};
    use tairix_resref::parse;

    use super::{read_value, MAX_VALUE_LEN};
    use crate::resinfo::MAX_INFO_VALUE_LEN;
    use crate::resolve::ResolveInfoError;
    use crate::transport::Transport;

    /// A stand-in broker serving exactly the three queries these tests read
    /// through: an ungated fact, a gated fact, and a counter. `deny` makes
    /// one query answer as the real service does when the caller lacks the
    /// capability the registry declares for it.
    struct Fixture {
        deny: Option<SysinfoQueryId>,
    }

    impl Fixture {
        fn serving() -> Self {
            Self { deny: None }
        }

        fn denying(query: SysinfoQueryId) -> Self {
            Self { deny: Some(query) }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            match header.query {
                SysinfoQueryId::SYSTEM_IDENTITY => {
                    Ok(SystemIdentity::new([0xAB; 16], 1, 2, 3, b"rustbox")
                        .expect("identity")
                        .to_le_bytes()
                        .to_vec())
                }
                SysinfoQueryId::KERNEL_MEMORY_STATS => Ok(KernelMemoryStats {
                    total_bytes: 8192,
                    free_bytes: 2048,
                    kernel_heap_bytes: 512,
                    user_resident_bytes: 4096,
                    page_size: 4096,
                    reserved: 0,
                }
                .to_le_bytes()
                .to_vec()),
                SysinfoQueryId::UPTIME => Ok(Uptime {
                    since_boot: Duration64::from_secs(4200),
                    boot_time: Time64::from_secs(1000),
                }
                .to_le_bytes()
                .to_vec()),
                _ => Err(Errno::NotFound),
            }
        }
    }

    fn read(reference: &str, fixture: &Fixture) -> Result<alloc::string::String, ResolveInfoError> {
        let parsed = parse(reference).expect("parse");
        read_value(&parsed, Time64::from_secs(5200), fixture)
    }

    /// Every value-backed payload kind reads as the bare value plus one
    /// newline — the line-shaped stream a text tool's line loop expects,
    /// with no label, unit, or envelope decoration to strip back off.
    #[test]
    fn a_served_reference_reads_as_the_bare_value_and_a_newline() {
        let fixture = Fixture::serving();
        // An ungated `info:` fact.
        assert_eq!(
            read("info:system/hostname", &fixture),
            Ok("rustbox\n".into())
        );
        // A gated `info:` fact — the case the bug report named.
        assert_eq!(read("info:mem/physical", &fixture), Ok("8192\n".into()));
        // A `stats:` counter renders as the figure, not the metric record.
        assert_eq!(read("stats:uptime", &fixture), Ok("4200\n".into()));
    }

    /// The rendering is the shared one, so a value read through a redirection
    /// is byte-identical to the same value printed by `sysinfo show` — the two
    /// spellings of one read can never disagree.
    #[test]
    fn the_value_matches_the_shared_show_rendering() {
        let fixture = Fixture::serving();
        let parsed = parse("info:mem/physical").expect("parse");
        let response =
            crate::resolve::resolve(&parsed, Time64::from_secs(5200), &fixture).expect("resolved");
        let mut want = response.display_value();
        want.push('\n');
        assert_eq!(read("info:mem/physical", &fixture), Ok(want));
    }

    /// A capability denial is a refusal, never an empty value: nothing is
    /// rendered and the error names the capability the frozen registry
    /// declares, so the shell can tell the user which grant to ask for.
    #[test]
    fn a_denied_read_names_the_capability_and_yields_no_value() {
        let fixture = Fixture::denying(SysinfoQueryId::KERNEL_MEMORY_STATS);
        let err = read("info:mem/physical", &fixture).expect_err("denied");
        assert_eq!(
            err,
            ResolveInfoError::CapabilityDenied(SysinfoQueryId::KERNEL_MEMORY_STATS)
        );
        assert_eq!(
            err.required_capability(),
            Some(CapabilityId::SYSINFO_KERNEL)
        );
        // The ungated sibling still reads: the denial is per-query, not a
        // blanket refusal of the namespace.
        assert_eq!(
            read("info:system/hostname", &fixture),
            Ok("rustbox\n".into())
        );
    }

    /// This path serves value-backed namespaces only. A *stream* namespace is
    /// the kernel resolver's job through `resource_open`, so routing one here
    /// is refused rather than served by a second reader — there is exactly one
    /// reader per backing.
    #[test]
    fn a_stream_namespace_is_not_served_here() {
        let fixture = Fixture::serving();
        for reference in ["sys:random", "sys:null", "disk:backup", "tty:debug"] {
            assert_eq!(
                read(reference, &fixture),
                Err(ResolveInfoError::NamespaceNotServed),
                "{reference} is stream-backed"
            );
        }
    }

    /// An unserved selector inside a served namespace fails closed, so a typo
    /// in a redirection target never feeds the child a fabricated value.
    #[test]
    fn an_unknown_selector_fails_closed() {
        let fixture = Fixture::serving();
        assert_eq!(
            read("info:nonsuch/leaf", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    /// A rate is undefined without its sampling window, and the refusal comes
    /// before any query is issued — a malformed request never reaches the
    /// service.
    #[test]
    fn a_rate_without_a_window_fails_closed() {
        let fixture = Fixture::serving();
        assert_eq!(
            read("stats:net/wan/rx.pps", &fixture),
            Err(ResolveInfoError::UnsupportedRequest)
        );
    }

    /// The bound is derived from the payload limits, not chosen, and stays far
    /// enough below one pipe's ring that the shell's pre-reader write can
    /// never block (`kernel/core/src/pipe.rs::PIPE_CAPACITY`, 64 KiB).
    #[test]
    fn the_length_bound_follows_the_payload_limit_and_fits_a_pipe() {
        assert_eq!(MAX_VALUE_LEN, MAX_INFO_VALUE_LEN + 1);
        const { assert!(MAX_VALUE_LEN < 64 * 1024) };
    }
}
