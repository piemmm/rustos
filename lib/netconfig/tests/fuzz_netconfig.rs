//! Deterministic fuzz harness for the `network.conf` store engine.
//!
//! Invariants, for any input bits an untrusted store file may carry:
//!
//! 1. [`NetworkConfig::parse`] never panics on any UTF-8 input, and
//!    respects its bounds (never returns more than [`MAX_INTERFACES`]
//!    interfaces, never a bond with more than [`MAX_BOND_MEMBERS`] members).
//! 2. A configuration that parses re-renders to a document that parses back
//!    **equal** ([`NetworkConfig::render`] → parse round-trip), and render
//!    never panics.
//! 3. Every rendered document is itself within the length bound, so a
//!    render can never produce a store the reader would reject as too long.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_netconfig::{NetworkConfig, MAX_BOND_MEMBERS, MAX_CONFIG_LEN, MAX_INTERFACES};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Lehmer-style LCG — deterministic, matches the sibling harnesses so a
/// failure reproduces one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        let index = usize::try_from(self.next_u64() % choices.len() as u64).expect("index fits");
        choices[index]
    }
}

/// The token soup a structured line is drawn from: real key suffixes and
/// values mixed with malformed ones, so generated documents walk the
/// accept/reject boundary rather than mostly failing at the first byte.
const IFACES: &[&str] = &[
    "wan", "lan0", "eth0", "eth1", "eth2", "bond0", "bond1", "0bad", "",
];
const SUFFIXES: &[&str] = &[
    "kind",
    "match.mac",
    "match.node",
    "ipv4.method",
    "ipv4.address",
    "ipv4.gateway",
    "ipv6.method",
    "ipv6.address",
    "ipv6.gateway",
    "mtu",
    "bond.members",
    "bond.mode",
    "bond.monitor-interval",
    "bond.primary",
    "bogus",
];
const VALUES: &[&str] = &[
    "ethernet",
    "bond",
    "loopback",
    "static",
    "slaac",
    "disabled",
    "active-backup",
    "balance",
    "52:54:00:00:00:01",
    "aa:bb",
    "10.0.0.1/24",
    "2001:db8::1/64",
    "10.0.0.1",
    "2001:db8::1",
    "1500",
    "9000",
    "1279",
    "eth0,eth1",
    "eth0,eth0",
    "200",
    "eth0",
    "virtio-net@0",
    "",
];

fn structured_line(rng: &mut Lcg) -> String {
    use std::fmt::Write as _;
    let mut line = String::new();
    // Occasionally a comment or blank line.
    match rng.next_u64() % 8 {
        0 => return String::from("# a comment"),
        1 => return String::new(),
        _ => {}
    }
    let _ = write!(
        line,
        "{}.{} {}",
        rng.pick(IFACES),
        rng.pick(SUFFIXES),
        rng.pick(VALUES),
    );
    line
}

fn build_document(rng: &mut Lcg) -> String {
    let lines = (rng.next_u64() % 40) as usize;
    let mut doc = String::new();
    for _ in 0..lines {
        doc.push_str(&structured_line(rng));
        doc.push('\n');
    }
    doc
}

fn check(doc: &str) {
    // (1) parse never panics, and honours its bounds.
    let Ok(config) = NetworkConfig::parse(doc) else {
        return;
    };
    assert!(config.interfaces().len() <= MAX_INTERFACES);
    for iface in config.interfaces() {
        assert!(iface.members().len() <= MAX_BOND_MEMBERS);
    }
    // (2) render/parse round-trips exactly, (3) within the length bound.
    let rendered = config.render();
    assert!(
        rendered.len() <= MAX_CONFIG_LEN,
        "rendered store exceeds the bound"
    );
    let reparsed = NetworkConfig::parse(&rendered).expect("a rendered store re-parses");
    assert_eq!(config, reparsed, "render/parse is not a round trip");
}

#[test]
fn structured_documents_round_trip_and_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "structured_documents_round_trip_and_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let doc = build_document(&mut rng);
            check(&doc);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn arbitrary_ascii_never_panics() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_ascii_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut buf = String::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            buf.clear();
            let len = (rng.next_u64() % 256) as usize;
            for _ in 0..len {
                // ASCII (incl. control chars, '.', ':', '/', digits, letters)
                // is always valid UTF-8, so `parse` sees a legal `&str`.
                let byte = (rng.next_u64() % 128) as u8;
                buf.push(byte as char);
            }
            // Must never panic; the result itself is irrelevant here.
            let _ = NetworkConfig::parse(&buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
