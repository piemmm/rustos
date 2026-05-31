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

## What is not here yet

`AGENTS.md` §19.3 also requires the SBOM to be **signed by the
per-installation key** (§11). That step is deliberately deferred: no
private-key signing API exists yet (`rustos-crypto` is verify-only, and
the local capability authority is a later stage). This command emits the
unsigned document the signer will later wrap. The remaining §19.3
items — the source-hash allow-list and advisory-SLA gate, the
`build --reproducible` verification, and the no-post-install-network-fetch
enforcement — are tracked in the `PLAN.md` "§19 Threat Model and
Hardening Burn-down".
