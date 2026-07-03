//! Deterministic fuzz harness for the `lib/procinfo` `info:`/`stats:` resolver
//! ([`rustos_procinfo::resolve`]).
//!
//! The resolver takes two untrusted inputs: the resource-reference string a
//! user or script supplied, and the reply bytes the System Information API
//! sends back (a compromised or buggy broker could return anything). The
//! harness's invariants are:
//!
//! * resolving any parsed reference against any reply never panics — it
//!   returns a [`ResourceResponse`](rustos_procinfo::ResourceResponse) or a
//!   typed [`ResolveInfoError`](rustos_procinfo::ResolveInfoError) (fail
//!   closed);
//! * a successful response is well-formed: the current envelope version, the
//!   `sysinfod` producer, and every bounded field within its limit;
//! * a reference outside `info:`/`stats:` is never resolved here.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG draws
//! reference strings (mutated real templates, delimiter splices, pure noise)
//! and drives a stand-in broker whose reply is itself PRNG-chosen between a
//! valid record, garbage bytes, and an error. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz` extends the loop to a wall-clock budget.

use core::cell::Cell;

use rustos_abi::origin::{CapabilitySummary, Origin, ProcId, TrustDomain};
use rustos_abi::sysinfo::{
    KernelMemoryStats, ResourceLimitRecord, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity,
    Uptime,
};
use rustos_abi::time::{Duration64, Time64};
use rustos_abi::{Errno, LimitKind, ResourceLimit};
use rustos_procinfo::{
    resolve, Producer, ResponsePayload, Transport, MAX_INFO_VALUE_LEN, MAX_METRIC_NAME_LEN,
    MAX_QUERY_LEN, RESINFO_VERSION_CURRENT,
};
use rustos_resref::parse;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string turned into a reference string.
const MAX_NOISE: usize = 256;

/// Real reference templates the harness mutates: served `info:`/`stats:`
/// selectors, unknown ones, decorated ones, and other namespaces.
const TEMPLATES: &[&str] = &[
    "info:system/hostname",
    "info:system/kernel",
    "info:system/machine-id",
    "info:system/boot-time",
    "info:process/pid",
    "info:process/uid",
    "info:process/gid",
    "info:process/proc-id",
    "info:process/trust-domain",
    "info:process/caps",
    "info:process/parent",
    "stats:uptime",
    "stats:mem/used",
    "stats:mem/available",
    "stats:mem/total",
    "stats:mem/kernel-heap",
    "stats:mem/user-resident",
    "info:mem/physical",
    "info:mem/page-size",
    "info:mem/used",
    "info:limits/address-space-bytes/soft",
    "info:limits/open-streams/hard",
    "info:limits/processes/soft",
    "info:limits/stack-bytes/hard",
    "stats:limits/address-space-bytes",
    "stats:limits/open-streams",
    "stats:limits/processes",
    "stats:limits/stack-bytes",
    "info:limits/nope/soft",
    "info:limits/processes/median",
    "stats:limits/nope",
    "info:system/nope",
    "stats:mem/pagefaults",
    "stats:cpu/load",
    "info:system/hostname::record",
    "stats:uptime?window=1s",
    "sys:random",
    "disk:backup@7K2M",
];

/// A stand-in `sysinfod` whose reply to each query is PRNG-chosen between a
/// valid record, arbitrary garbage bytes, and a transport error — so the
/// resolver's decode and error paths are all exercised.
struct HostileBroker {
    state: Cell<u64>,
}

impl HostileBroker {
    fn new(seed: u64) -> Self {
        Self {
            state: Cell::new(seed | 1),
        }
    }

    fn next(&self) -> u64 {
        let s = self
            .state
            .get()
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state.set(s);
        s
    }

    fn valid_reply(query: SysinfoQueryId) -> Option<Vec<u8>> {
        match query {
            SysinfoQueryId::SYSTEM_IDENTITY => Some(
                SystemIdentity::new([0x5A; 16], 9, 8, 7, b"fuzzbox")
                    .ok()?
                    .to_le_bytes()
                    .to_vec(),
            ),
            SysinfoQueryId::UPTIME => Some(
                Uptime {
                    since_boot: Duration64::from_secs(12_345),
                    boot_time: Time64::from_secs(1),
                }
                .to_le_bytes()
                .to_vec(),
            ),
            SysinfoQueryId::KERNEL_MEMORY_STATS => Some(
                KernelMemoryStats {
                    total_bytes: 1 << 30,
                    free_bytes: 1 << 20,
                    kernel_heap_bytes: 4096,
                    user_resident_bytes: 8192,
                    page_size: 4096,
                    reserved: 0,
                }
                .to_le_bytes()
                .to_vec(),
            ),
            SysinfoQueryId::PROCESS_IDENTITY => Some(
                Origin::new(
                    TrustDomain::User,
                    1000,
                    50,
                    42,
                    ProcId::from_raw([0x5A; 16]),
                    CapabilitySummary::EMPTY,
                    rustos_abi::ORIGIN_CONSOLE_NONE,
                )
                .to_le_bytes()
                .to_vec(),
            ),
            SysinfoQueryId::RESOURCE_LIMITS => {
                // The four records in `LimitKind` discriminant order, as the
                // real service frames them.
                let mut out = Vec::new();
                for kind in LimitKind::ALL {
                    out.extend_from_slice(
                        &ResourceLimitRecord::new(kind, ResourceLimit::UNLIMITED, 1).to_le_bytes(),
                    );
                }
                Some(out)
            }
            _ => None,
        }
    }
}

impl Transport for HostileBroker {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        // The request is always framed by the resolver, so the header decodes;
        // a decode failure here would itself be a bug worth surfacing.
        let header = SysinfoRequestHeader::from_bytes(request)?;
        match self.next() % 4 {
            // A well-formed record for a known query, else an empty reply.
            0 | 1 => Ok(Self::valid_reply(header.query).unwrap_or_default()),
            // Arbitrary garbage: a random-length run of pseudo-random bytes.
            2 => {
                let len = usize::try_from(self.next() % 200).unwrap_or(0);
                Ok((0..len).map(|_| self.next().to_le_bytes()[0]).collect())
            }
            // A transport error (including a capability denial).
            _ => Err(if self.next() & 1 == 0 {
                Errno::PermissionDenied
            } else {
                Errno::NotFound
            }),
        }
    }
}

/// `x` reduced into `0..=max`.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Low byte of `x`.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// Parse `input` (never panics) and, when it parses, resolve it against a
/// hostile broker and check the response invariants.
fn exercise(input: &str, broker: &HostileBroker) {
    let Ok(reference) = parse(input) else {
        return;
    };
    let now = Time64::from_secs(1_000);
    let Ok(response) = resolve(&reference, now, broker) else {
        return;
    };
    // A successful response is well-formed.
    assert_eq!(response.version, RESINFO_VERSION_CURRENT);
    assert_eq!(response.producer, Producer::Sysinfod);
    assert!(response.query().len() <= MAX_QUERY_LEN);
    match &response.payload {
        ResponsePayload::Info(v) => assert!(v.value().len() <= MAX_INFO_VALUE_LEN),
        ResponsePayload::Metric(m) => assert!(m.name().len() <= MAX_METRIC_NAME_LEN),
    }
}

#[test]
fn resolve_never_panics_and_stays_well_formed() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = rustos_fuzzseed::start(
        "resolve_never_panics_and_stays_well_formed",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        let broker = HostileBroker::new(next());

        // 1. A real template with a handful of bytes flipped at random.
        let template = TEMPLATES[bounded(next(), TEMPLATES.len() - 1)];
        let mut mutated: Vec<u8> = template.as_bytes().to_vec();
        let flips = bounded(next(), 5);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise(&String::from_utf8_lossy(&mutated), &broker);

        // 2. A structured-but-hostile string exercising the delimiter split.
        let blob_len = bounded(next(), 40);
        let mut spliced = String::new();
        for _ in 0..blob_len {
            match bounded(next(), 6) {
                0 => spliced.push(':'),
                1 => spliced.push('/'),
                2 => spliced.push('@'),
                3 => spliced.push_str("::"),
                4 => spliced.push('?'),
                5 => spliced.push_str("info"),
                _ => spliced.push(char::from(b'a' + low_byte(next() >> 29) % 26)),
            }
        }
        exercise(&spliced, &broker);

        // 3. Pure noise (lossy UTF-8).
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        exercise(&String::from_utf8_lossy(&noise), &broker);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
