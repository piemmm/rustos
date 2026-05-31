# Supply-chain integrity: the SBOM

Every external crate is code RustOS neither wrote nor fully controls; it
widens the trusted computing base and the audit burden (`AGENTS.md`
§2.12). `AGENTS.md` §19.3 requires that every image embed a CycloneDX
Software Bill of Materials (SBOM) listing **every workspace and external
crate by version, source URL, and source checksum**, so a deployed
system can be matched against published advisories and so a tampered
dependency (the xz-utils class of attack) is detectable.

`cargo xtask sbom` produces that document.

## Source of truth

The SBOM is generated from the committed `Cargo.lock`. Cargo's lockfile
is already the authoritative, pinned record of the resolved dependency
graph: each `[[package]]` block carries the crate name, the exact
resolved version, its `source`, and — for registry crates — the
`checksum` (a registry SHA-256). Deriving the SBOM from the lockfile
means the bill of materials and the bytes the build actually consumes
cannot drift apart.

The generator is self-contained (`AGENTS.md` §2.12): it parses the
lockfile and emits CycloneDX JSON directly, with no `serde`/`cyclonedx`
dependency and without shelling out to `cargo metadata`.

## Output

`cargo xtask sbom` writes a CycloneDX 1.5 JSON document to stdout, or to
a file with `--output PATH` (`-o PATH`); missing parent directories are
created so the document can be dropped straight into the gitignored
`images/` tree.

Each resolved package becomes one `library` component carrying:

* a package URL (`purl`) of the form `pkg:cargo/<name>@<version>`, used
  as the component `bom-ref`;
* a `SHA-256` hash entry holding the registry checksum, when the source
  pins one;
* a `distribution` external reference holding the source URL (Cargo's
  `registry+` / `git+` scheme prefix stripped);
* a `rustos:source-class` property marking the crate `workspace`,
  `registry`, `git`, or `other`, so first-party code is distinguishable
  from the external attack surface at a glance.

The root component identifies the workspace itself.

The output is **deterministic**: components are sorted by name and
version, and no timestamp or random serial number is emitted. This is a
prerequisite for the reproducible-build verification tracked as a later
§19.3 burn-down item — two runs against the same lockfile produce byte-
identical SBOMs.

## Enforcement: source-hash pinning and the advisory SLA

The SBOM records what the build consumed; `cargo xtask supply-chain`
*enforces* two further §19.3 controls against a single committed policy
file, `supply-chain.toml`, at the workspace root. It runs as part of
`cargo xtask ci`, immediately after `cargo deny`.

## Source-hash allow-list

`Cargo.lock` already pins every external-registry crate's tarball
SHA-256, and Cargo verifies downloads against it — but the lockfile is
also exactly where a hostile dependency bump would land. §19.3 therefore
requires a **separate, independently reviewed** allow-list of those
hashes.

Each `[[source-pin]]` block in `supply-chain.toml` pins one crate's
`name`, exact `version`, and `sha256`. The check fails, closed, when:

* a registry crate in `Cargo.lock` has **no pin** (a new dependency must
  be vetted and added before it can build) — the error quotes the exact
  block to paste in;
* a pin's hash **does not match** the lockfile (the xz-utils class of
  tamper);
* a pin is **stale** (its crate is no longer in `Cargo.lock`) or
  duplicated.

The pins are generated from the lockfile with `cargo xtask supply-chain
--write-pins` and committed. Like `Cargo.lock` itself the file is
generated but its **diff must be reviewed**: a dependency or hash change
shows up in a dedicated security artefact, so the two files must move
together and neither can change silently.

## Advisory SLA

A RUSTSEC advisory against a workspace dependency blocks every merge but
the resolving bump. `cargo deny` blocks the advisory immediately; the
`[[advisory]]` ledger here governs the *grace window* in which one may be
temporarily accepted while that bump is prepared. Each entry records the
advisory `id`, the affected `package`, its `published` date
(`YYYY-MM-DD`), a `tier`, and a `reason`.

The `tier` selects the SLA from `AGENTS.md` §19.3:

* `crypto` — a dependency of `lib/crypto`: **7 days** from publication;
* `general` — any other crate: **30 days** from publication.

The check fails, closed, the day after an accepted advisory exceeds its
SLA (age is measured in whole days; exactly the SLA is still within it).
The ledger is empty today — no advisory affects a workspace dependency.

## What is not here yet

`AGENTS.md` §19.3 also requires the SBOM to be **signed by the
per-installation key** (§11). That step is deliberately deferred: no
private-key signing API exists yet (`rustos-crypto` is verify-only, and
the local capability authority is a later stage). This command emits the
unsigned document the signer will later wrap. The remaining §19.3
items — the `build --reproducible` verification and the
no-post-install-network-fetch enforcement — are tracked in the `PLAN.md`
"§19 Threat Model and Hardening Burn-down".
