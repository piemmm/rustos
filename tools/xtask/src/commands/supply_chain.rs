//! `cargo xtask supply-chain` implementation (`PLAN.md` §19 burn-down item
//! 4).
//!
//! the charter mandates two complementary supply-chain controls beyond the SBOM:
//!
//! * **Source-hash pinning.** `Cargo.lock` already records each external
//!   crate's registry tarball SHA-256, but the lockfile is also where a
//!   hostile dependency bump would land. So the charter requires a *separate*,
//!   independently reviewed allow-list of those hashes: a crate whose
//!   `Cargo.lock` checksum does not match its pinned value fails the
//!   build, and a new dependency that is not yet pinned fails the build
//!   until it is vetted and added. The two files must be changed together,
//!   so any dependency or hash change is visible in the diff of a
//!   dedicated security artefact.
//! * **Advisory SLA.** A RUSTSEC advisory against a workspace dependency
//!   has a 7-day SLA (dependencies of `lib/crypto`) or a 30-day SLA (all
//!   other crates) from publication. `cargo deny` blocks the advisory
//!   immediately; the SLA ledger here caps how long an advisory may be
//!   *accepted* (the grace window before the resolving bump lands) and
//!   fails the build, closed, once that window is exceeded.
//!
//! Both controls read a single committed policy file, `supply-chain.toml`,
//! at the workspace root. The parser and the JSON-free style mirror
//! [`super::sbom`] (roll your own; no `toml`/`serde`
//! dependency). The `[[source-pin]]` blocks are regenerated from
//! `Cargo.lock` with `--write-pins` (reviewed by diff, exactly like the
//! lockfile itself); the `[[advisory]]` blocks are hand-curated.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use super::sbom::{parse_cargo_lock, parse_key, LockedPackage};

/// The committed supply-chain policy file, relative to the workspace root.
pub const POLICY_FILE: &str = "supply-chain.toml";

/// One pinned external-registry crate: name, exact version, and the
/// registry tarball SHA-256 the build is allowed to consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePin {
    pub name: String,
    pub version: String,
    pub sha256: String,
}

/// The SLA tier an advisory falls under, deciding its grace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryTier {
    /// A dependency of `lib/crypto`: 7-day SLA from publication.
    Crypto,
    /// Any other workspace dependency: 30-day SLA from publication.
    General,
}

impl AdvisoryTier {
    /// The grace window, in days from publication, before the advisory
    /// blocks every merge.
    pub fn sla_days(self) -> i64 {
        match self {
            AdvisoryTier::Crypto => 7,
            AdvisoryTier::General => 30,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "crypto" => Ok(AdvisoryTier::Crypto),
            "general" => Ok(AdvisoryTier::General),
            other => Err(format!(
                "unknown advisory tier `{other}` (expected `crypto` or `general`)"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            AdvisoryTier::Crypto => "crypto",
            AdvisoryTier::General => "general",
        }
    }
}

/// One temporarily-accepted RUSTSEC advisory awaiting its resolving bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryEntry {
    pub id: String,
    pub package: String,
    /// Publication date in `YYYY-MM-DD` form.
    pub published: String,
    pub tier: AdvisoryTier,
    pub reason: String,
}

/// The parsed contents of `supply-chain.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Policy {
    pub pins: Vec<SourcePin>,
    pub advisories: Vec<AdvisoryEntry>,
}

/// A block being accumulated while parsing the policy file.
enum Block {
    Pin(PartialPin),
    Advisory(PartialAdvisory),
}

#[derive(Default)]
struct PartialPin {
    name: Option<String>,
    version: Option<String>,
    sha256: Option<String>,
}

#[derive(Default)]
struct PartialAdvisory {
    id: Option<String>,
    package: Option<String>,
    published: Option<String>,
    tier: Option<String>,
    reason: Option<String>,
}

/// Parse `supply-chain.toml` into its [`Policy`].
///
/// The file is a sequence of `[[source-pin]]` and `[[advisory]]` blocks,
/// each with `key = "value"` lines. Blank lines and `#` comments are
/// ignored. A block missing a required key, an unknown key, or a stray
/// key outside any block is a hard error so the policy can never silently
/// drop a pin or an advisory.
pub fn parse_policy(text: &str) -> Result<Policy, String> {
    let mut policy = Policy::default();
    let mut block: Option<Block> = None;

    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[source-pin]]" {
            finish_block(block.take(), &mut policy, lineno)?;
            block = Some(Block::Pin(PartialPin::default()));
            continue;
        }
        if line == "[[advisory]]" {
            finish_block(block.take(), &mut policy, lineno)?;
            block = Some(Block::Advisory(PartialAdvisory::default()));
            continue;
        }
        if line.starts_with('[') {
            return Err(format!(
                "{POLICY_FILE}: unexpected table header on line {lineno}: `{line}` \
                 (only [[source-pin]] and [[advisory]] are allowed)"
            ));
        }
        let Some(current) = block.as_mut() else {
            return Err(format!(
                "{POLICY_FILE}: stray key on line {lineno} outside any block: `{line}`"
            ));
        };
        set_key(current, line, lineno)?;
    }
    finish_block(block.take(), &mut policy, text.lines().count() + 1)?;
    Ok(policy)
}

fn set_key(block: &mut Block, line: &str, lineno: usize) -> Result<(), String> {
    match block {
        Block::Pin(pin) => {
            if let Some(v) = parse_key(line, "name") {
                pin.name = Some(v);
            } else if let Some(v) = parse_key(line, "version") {
                pin.version = Some(v);
            } else if let Some(v) = parse_key(line, "sha256") {
                pin.sha256 = Some(v);
            } else {
                return Err(unknown_key(line, lineno, "source-pin"));
            }
        }
        Block::Advisory(adv) => {
            if let Some(v) = parse_key(line, "id") {
                adv.id = Some(v);
            } else if let Some(v) = parse_key(line, "package") {
                adv.package = Some(v);
            } else if let Some(v) = parse_key(line, "published") {
                adv.published = Some(v);
            } else if let Some(v) = parse_key(line, "tier") {
                adv.tier = Some(v);
            } else if let Some(v) = parse_key(line, "reason") {
                adv.reason = Some(v);
            } else {
                return Err(unknown_key(line, lineno, "advisory"));
            }
        }
    }
    Ok(())
}

fn unknown_key(line: &str, lineno: usize, block: &str) -> String {
    format!("{POLICY_FILE}: line {lineno} in [[{block}]] is not a recognised key: `{line}`")
}

fn finish_block(block: Option<Block>, policy: &mut Policy, lineno: usize) -> Result<(), String> {
    match block {
        None => Ok(()),
        Some(Block::Pin(pin)) => {
            let name = require(pin.name, "name", "source-pin", lineno)?;
            let version = require(pin.version, "version", "source-pin", lineno)?;
            let sha256 = require(pin.sha256, "sha256", "source-pin", lineno)?;
            policy.pins.push(SourcePin {
                name,
                version,
                sha256,
            });
            Ok(())
        }
        Some(Block::Advisory(adv)) => {
            let id = require(adv.id, "id", "advisory", lineno)?;
            let package = require(adv.package, "package", "advisory", lineno)?;
            let published = require(adv.published, "published", "advisory", lineno)?;
            let tier = require(adv.tier, "tier", "advisory", lineno)?;
            let reason = require(adv.reason, "reason", "advisory", lineno)?;
            policy.advisories.push(AdvisoryEntry {
                id,
                package,
                published,
                tier: AdvisoryTier::parse(&tier)?,
                reason,
            });
            Ok(())
        }
    }
}

fn require(value: Option<String>, key: &str, block: &str, lineno: usize) -> Result<String, String> {
    value.ok_or_else(|| format!("{POLICY_FILE}: [[{block}]] before line {lineno} has no `{key}`"))
}

/// Verify every external-registry crate in `locked` is pinned with a
/// matching SHA-256, and that no pin is stale.
///
/// All problems are reported together so a contributor sees the complete
/// set of pins to add, fix, or remove in one pass.
pub fn check_source_pins(locked: &[LockedPackage], pins: &[SourcePin]) -> Result<(), String> {
    let mut pinned: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    let mut problems: Vec<String> = Vec::new();
    for pin in pins {
        if pinned
            .insert((&pin.name, &pin.version), &pin.sha256)
            .is_some()
        {
            problems.push(format!(
                "duplicate source-pin for {} {}",
                pin.name, pin.version
            ));
        }
    }

    for pkg in locked {
        if !pkg.is_external_registry() {
            continue;
        }
        let Some(checksum) = pkg.checksum.as_deref() else {
            problems.push(format!(
                "registry crate {} {} has no checksum in Cargo.lock",
                pkg.name, pkg.version
            ));
            continue;
        };
        match pinned.get(&(pkg.name.as_str(), pkg.version.as_str())) {
            None => problems.push(format!(
                "unpinned dependency {} {}; vet it and add to {POLICY_FILE}:\n      \
                 [[source-pin]]\n      name = \"{}\"\n      version = \"{}\"\n      \
                 sha256 = \"{}\"",
                pkg.name, pkg.version, pkg.name, pkg.version, checksum
            )),
            Some(expected) if *expected != checksum => problems.push(format!(
                "source-hash mismatch for {} {}: Cargo.lock has {checksum}, {POLICY_FILE} pins {expected}",
                pkg.name, pkg.version
            )),
            Some(_) => {}
        }
    }

    for pin in pins {
        let present = locked.iter().any(|pkg| {
            pkg.is_external_registry() && pkg.name == pin.name && pkg.version == pin.version
        });
        if !present {
            problems.push(format!(
                "stale source-pin {} {}: no longer in Cargo.lock; remove it from {POLICY_FILE}",
                pin.name, pin.version
            ));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "source-hash allow-list check failed ({} problem(s)):\n  - {}",
            problems.len(),
            problems.join("\n  - ")
        ))
    }
}

/// Fail, closed, for every accepted advisory whose age exceeds its SLA.
///
/// `today_days` is the current date as a count of days since the Unix
/// epoch (see [`days_from_civil`]); taking it as a parameter keeps the
/// decision a pure, testable function independent of the wall clock.
pub fn evaluate_advisory_sla(advisories: &[AdvisoryEntry], today_days: i64) -> Result<(), String> {
    let mut violations: Vec<String> = Vec::new();
    for adv in advisories {
        let published = parse_date(&adv.published)
            .map_err(|e| format!("advisory {} has an invalid `published` date: {e}", adv.id))?;
        let age = today_days - published;
        let sla = adv.tier.sla_days();
        if age > sla {
            violations.push(format!(
                "{} against `{}` is {age} days old, past its {sla}-day {} SLA \
                 (published {}); land the resolving bump",
                adv.id,
                adv.package,
                adv.tier.as_str(),
                adv.published
            ));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "advisory SLA exceeded ({} advisory/advisories):\n  - {}",
            violations.len(),
            violations.join("\n  - ")
        ))
    }
}

/// Days from `1970-01-01` to the civil date `(y, m, d)`, negative before
/// the epoch. Howard Hinnant's `days_from_civil` algorithm; valid for any
/// proleptic-Gregorian date. Only the *difference* between two such counts
/// is used here, so leap years and month lengths are handled exactly
/// without a calendar table.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse a `YYYY-MM-DD` date into days since the Unix epoch.
pub fn parse_date(text: &str) -> Result<i64, String> {
    let mut parts = text.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("`{text}` is not a YYYY-MM-DD date"));
    };
    let year = y
        .parse::<i64>()
        .map_err(|_| format!("`{text}`: year `{y}` is not a number"))?;
    let month = m
        .parse::<i64>()
        .map_err(|_| format!("`{text}`: month `{m}` is not a number"))?;
    let day = d
        .parse::<i64>()
        .map_err(|_| format!("`{text}`: day `{d}` is not a number"))?;
    if !(1..=12).contains(&month) {
        return Err(format!("`{text}`: month {month} out of range 1..=12"));
    }
    if !(1..=31).contains(&day) {
        return Err(format!("`{text}`: day {day} out of range 1..=31"));
    }
    Ok(days_from_civil(year, month, day))
}

/// Derive the `[[source-pin]]` set from the resolved lockfile packages.
/// `locked` is expected pre-sorted (as returned by [`parse_cargo_lock`]),
/// so the generated list is deterministic.
pub fn pins_from_lock(locked: &[LockedPackage]) -> Vec<SourcePin> {
    locked
        .iter()
        .filter(|p| p.is_external_registry())
        .filter_map(|p| {
            p.checksum.as_ref().map(|sum| SourcePin {
                name: p.name.clone(),
                version: p.version.clone(),
                sha256: sum.clone(),
            })
        })
        .collect()
}

/// Render a complete `supply-chain.toml`: the fixed header, the generated
/// `[[source-pin]]` blocks, then the preserved `[[advisory]]` blocks.
pub fn render_policy(pins: &[SourcePin], advisories: &[AdvisoryEntry]) -> String {
    let mut out = String::new();
    out.push_str(POLICY_HEADER);
    for pin in pins {
        // `write!` into a `String` is infallible; the result is discarded.
        let _ = write!(
            out,
            "\n[[source-pin]]\nname = \"{}\"\nversion = \"{}\"\nsha256 = \"{}\"\n",
            pin.name, pin.version, pin.sha256
        );
    }
    for adv in advisories {
        let _ = write!(
            out,
            "\n[[advisory]]\nid = \"{}\"\npackage = \"{}\"\npublished = \"{}\"\n\
             tier = \"{}\"\nreason = \"{}\"\n",
            adv.id,
            adv.package,
            adv.published,
            adv.tier.as_str(),
            adv.reason
        );
    }
    out
}

const POLICY_HEADER: &str = "\
# RustOS supply-chain policy (AGENTS.md §19.3, PLAN.md §19 item 4).
#
# Verified by `cargo xtask supply-chain` (run as part of `cargo xtask ci`).
#
# [[source-pin]] blocks pin every external-registry crate's tarball
# SHA-256. They are regenerated from Cargo.lock with
# `cargo xtask supply-chain --write-pins` and committed: like Cargo.lock,
# the file is generated but its diff MUST be reviewed, so a dependency or
# hash change is visible in a dedicated security artefact. The check fails
# if a crate is unpinned, mismatched, or a pin is stale.
#
# [[advisory]] blocks are hand-curated. Each records a RUSTSEC advisory
# temporarily accepted while its resolving bump is prepared. `tier` is
# `crypto` (a lib/crypto dependency, 7-day SLA) or `general` (30-day SLA);
# the check fails, closed, once an advisory is older than its SLA.
";

/// Current date as days since the Unix epoch, from the system clock.
fn today_days() -> Result<i64, String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
        .as_secs();
    i64::try_from(secs / 86_400)
        .map_err(|_| "system clock is implausibly far in the future".to_string())
}

/// Run the supply-chain checks (or regenerate the pins with `write_pins`).
pub fn run(workspace_root: &Path, write_pins: bool) -> Result<(), String> {
    let lock_path = workspace_root.join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("supply-chain: cannot read {}: {e}", lock_path.display()))?;
    let locked = parse_cargo_lock(&lock)?;
    let policy_path = workspace_root.join(POLICY_FILE);

    if write_pins {
        let advisories = if policy_path.exists() {
            let text = std::fs::read_to_string(&policy_path)
                .map_err(|e| format!("supply-chain: cannot read {}: {e}", policy_path.display()))?;
            parse_policy(&text)?.advisories
        } else {
            Vec::new()
        };
        let pins = pins_from_lock(&locked);
        let document = render_policy(&pins, &advisories);
        std::fs::write(&policy_path, document.as_bytes())
            .map_err(|e| format!("supply-chain: cannot write {}: {e}", policy_path.display()))?;
        eprintln!(
            "xtask: [supply-chain] wrote {} pins and preserved {} advisories to {}",
            pins.len(),
            advisories.len(),
            policy_path.display()
        );
        return Ok(());
    }

    let text = std::fs::read_to_string(&policy_path).map_err(|e| {
        format!(
            "supply-chain: cannot read {}: {e}; run `cargo xtask supply-chain --write-pins` \
             to create it",
            policy_path.display()
        )
    })?;
    let policy = parse_policy(&text)?;
    check_source_pins(&locked, &policy.pins)?;
    evaluate_advisory_sla(&policy.advisories, today_days()?)?;
    eprintln!(
        "xtask: [supply-chain] {} pins verified against Cargo.lock, {} advisories within SLA",
        policy.pins.len(),
        policy.advisories.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::sbom::parse_cargo_lock;

    fn registry_pkg(name: &str, version: &str, checksum: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            checksum: Some(checksum.to_string()),
        }
    }

    fn workspace_pkg(name: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: "0.0.0".to_string(),
            source: None,
            checksum: None,
        }
    }

    fn pin(name: &str, version: &str, sha256: &str) -> SourcePin {
        SourcePin {
            name: name.to_string(),
            version: version.to_string(),
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn parse_policy_reads_pins_and_advisories() {
        let text = r#"
# header comment
[[source-pin]]
name = "memchr"
version = "2.7.4"
sha256 = "deadbeef"

[[advisory]]
id = "RUSTSEC-2024-0001"
package = "memchr"
published = "2024-03-01"
tier = "general"
reason = "fix queued in PR #42"
"#;
        let policy = parse_policy(text).expect("parse");
        assert_eq!(policy.pins, vec![pin("memchr", "2.7.4", "deadbeef")]);
        assert_eq!(policy.advisories.len(), 1);
        let adv = &policy.advisories[0];
        assert_eq!(adv.id, "RUSTSEC-2024-0001");
        assert_eq!(adv.package, "memchr");
        assert_eq!(adv.tier, AdvisoryTier::General);
    }

    #[test]
    fn parse_policy_rejects_unknown_key() {
        let text = "[[source-pin]]\nname = \"x\"\nflavour = \"vanilla\"\n";
        let err = parse_policy(text).unwrap_err();
        assert!(err.contains("not a recognised key"), "{err}");
    }

    #[test]
    fn parse_policy_rejects_missing_required_key() {
        let text = "[[source-pin]]\nname = \"x\"\nversion = \"1.0.0\"\n";
        let err = parse_policy(text).unwrap_err();
        assert!(err.contains("has no `sha256`"), "{err}");
    }

    #[test]
    fn parse_policy_rejects_unknown_tier() {
        let text = "[[advisory]]\nid = \"R\"\npackage = \"p\"\npublished = \"2024-01-01\"\n\
                    tier = \"silver\"\nreason = \"r\"\n";
        let err = parse_policy(text).unwrap_err();
        assert!(err.contains("unknown advisory tier"), "{err}");
    }

    #[test]
    fn parse_policy_rejects_stray_key() {
        let err = parse_policy("name = \"orphan\"\n").unwrap_err();
        assert!(err.contains("stray key"), "{err}");
    }

    #[test]
    fn check_source_pins_accepts_matching_set() {
        let locked = vec![
            workspace_pkg("rustos-abi"),
            registry_pkg("memchr", "2.7.4", "abc123"),
        ];
        let pins = vec![pin("memchr", "2.7.4", "abc123")];
        assert!(check_source_pins(&locked, &pins).is_ok());
    }

    #[test]
    fn check_source_pins_flags_unpinned_dependency() {
        let locked = vec![registry_pkg("memchr", "2.7.4", "abc123")];
        let err = check_source_pins(&locked, &[]).unwrap_err();
        assert!(err.contains("unpinned dependency memchr 2.7.4"), "{err}");
        // The error is paste-ready: it quotes the actual checksum.
        assert!(err.contains("sha256 = \"abc123\""), "{err}");
    }

    #[test]
    fn check_source_pins_flags_hash_mismatch() {
        let locked = vec![registry_pkg("memchr", "2.7.4", "abc123")];
        let pins = vec![pin("memchr", "2.7.4", "0000")];
        let err = check_source_pins(&locked, &pins).unwrap_err();
        assert!(
            err.contains("source-hash mismatch for memchr 2.7.4"),
            "{err}"
        );
    }

    #[test]
    fn check_source_pins_flags_stale_pin() {
        let locked = vec![registry_pkg("memchr", "2.7.4", "abc123")];
        let pins = vec![
            pin("memchr", "2.7.4", "abc123"),
            pin("removed-crate", "1.0.0", "ff"),
        ];
        let err = check_source_pins(&locked, &pins).unwrap_err();
        assert!(
            err.contains("stale source-pin removed-crate 1.0.0"),
            "{err}"
        );
    }

    #[test]
    fn check_source_pins_flags_duplicate_pin() {
        let locked = vec![registry_pkg("memchr", "2.7.4", "abc123")];
        let pins = vec![
            pin("memchr", "2.7.4", "abc123"),
            pin("memchr", "2.7.4", "abc123"),
        ];
        let err = check_source_pins(&locked, &pins).unwrap_err();
        assert!(
            err.contains("duplicate source-pin for memchr 2.7.4"),
            "{err}"
        );
    }

    #[test]
    fn days_from_civil_matches_known_anchors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // 2000-03-01 is 11017 days after the epoch (crosses a leap year).
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2024, 1, 1), 19723);
    }

    #[test]
    fn parse_date_rejects_malformed_input() {
        assert!(parse_date("2024-13-01").is_err());
        assert!(parse_date("2024-01-32").is_err());
        assert!(parse_date("2024-01").is_err());
        assert!(parse_date("not-a-date").is_err());
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
    }

    fn advisory(id: &str, published: &str, tier: AdvisoryTier) -> AdvisoryEntry {
        AdvisoryEntry {
            id: id.to_string(),
            package: "some-dep".to_string(),
            published: published.to_string(),
            tier,
            reason: "bump queued".to_string(),
        }
    }

    #[test]
    fn advisory_within_sla_passes() {
        let today = days_from_civil(2024, 1, 20);
        // General: 30-day SLA, published 19 days ago.
        let advisories = vec![advisory(
            "RUSTSEC-2024-0001",
            "2024-01-01",
            AdvisoryTier::General,
        )];
        assert!(evaluate_advisory_sla(&advisories, today).is_ok());
    }

    #[test]
    fn crypto_advisory_past_seven_days_fails() {
        let today = days_from_civil(2024, 1, 20);
        let advisories = vec![advisory(
            "RUSTSEC-2024-0002",
            "2024-01-01",
            AdvisoryTier::Crypto,
        )];
        let err = evaluate_advisory_sla(&advisories, today).unwrap_err();
        assert!(err.contains("7-day crypto SLA"), "{err}");
        assert!(err.contains("RUSTSEC-2024-0002"), "{err}");
    }

    #[test]
    fn general_advisory_at_exactly_thirty_days_passes() {
        let published = days_from_civil(2024, 1, 1);
        let advisories = vec![advisory(
            "RUSTSEC-2024-0003",
            "2024-01-01",
            AdvisoryTier::General,
        )];
        // Exactly 30 days old is still within the SLA; day 31 trips it.
        assert!(evaluate_advisory_sla(&advisories, published + 30).is_ok());
        assert!(evaluate_advisory_sla(&advisories, published + 31).is_err());
    }

    #[test]
    fn future_dated_advisory_is_not_a_violation() {
        let today = days_from_civil(2024, 1, 1);
        let advisories = vec![advisory(
            "RUSTSEC-2024-0004",
            "2024-06-01",
            AdvisoryTier::Crypto,
        )];
        assert!(evaluate_advisory_sla(&advisories, today).is_ok());
    }

    #[test]
    fn render_policy_round_trips_through_parser() {
        let pins = vec![
            pin("memchr", "2.7.4", "abc123"),
            pin("libc", "0.2.1", "def456"),
        ];
        let advisories = vec![advisory(
            "RUSTSEC-2024-0001",
            "2024-03-01",
            AdvisoryTier::Crypto,
        )];
        let rendered = render_policy(&pins, &advisories);
        let parsed = parse_policy(&rendered).expect("re-parse rendered policy");
        assert_eq!(parsed.pins, pins);
        assert_eq!(parsed.advisories, advisories);
    }

    #[test]
    fn write_pins_is_deterministic_and_idempotent() {
        let pins = pins_from_lock(&[
            registry_pkg("memchr", "2.7.4", "abc123"),
            workspace_pkg("rustos-abi"),
        ]);
        assert_eq!(pins, vec![pin("memchr", "2.7.4", "abc123")]);
        let first = render_policy(&pins, &[]);
        let second = render_policy(&pins, &[]);
        assert_eq!(first, second);
    }

    /// The committed policy must verify against the live `Cargo.lock`:
    /// every external-registry crate is pinned, no pin is stale, and every
    /// advisory (if any) is within its SLA at the time it was committed.
    #[test]
    fn committed_policy_matches_workspace_lockfile() {
        let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.pop(); // tools
        root.pop(); // workspace
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
        let locked = parse_cargo_lock(&lock).expect("parse Cargo.lock");
        let policy_text =
            std::fs::read_to_string(root.join(POLICY_FILE)).expect("read supply-chain.toml");
        let policy = parse_policy(&policy_text).expect("parse policy");
        check_source_pins(&locked, &policy.pins).expect("committed pins match Cargo.lock");
        // Regenerating the pins must reproduce exactly what is committed.
        assert_eq!(pins_from_lock(&locked), policy.pins);
    }
}
