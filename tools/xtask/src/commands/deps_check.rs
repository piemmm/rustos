//! `cargo xtask deps-check` implementation (`AGENTS.md` §17.4 / §17.5).
//!
//! §17.5 requires a check that walks the workspace dependency graph and
//! fails the build when any of these holds:
//!
//! 1. the §17.4 layering graph is violated,
//! 2. a non-GUI crate transitively depends on `userland/gui/*`, or
//! 3. a kernel crate outside `kernel/sched/*` / `kernel/core` names a
//!    concrete scheduler crate.
//!
//! The graph is reconstructed from the workspace member manifests rather
//! than from `cargo metadata` JSON: every in-workspace edge is a `path =`
//! dependency, so the manifests are the authoritative, dependency-free
//! source of truth (`AGENTS.md` §2.12 — roll our own; no new external
//! crate just to parse JSON). Only *build-graph* dependencies are
//! considered — `[dev-dependencies]` are test-only scaffolding and are
//! excluded, matching the production layering §17.4 describes.
//!
//! ## Interpretation of §17.4
//!
//! §17.4 polices the *cross-stratum* boundaries: `lib` → `kernel` →
//! `drivers`/`userland`, the `api`/`impl` split that makes the scheduler
//! and the architecture pluggable, and the one-way edge that keeps the
//! desktop optional. Edges *within* the kernel-subsystem stratum (e.g.
//! `ipc` → `mem`) are the kernel's internal wiring, not a stratum
//! crossing, and are permitted. The matrix in [`layer_allows`] encodes
//! exactly the strata of §17.4.
//!
//! ## Grandfathered violations
//!
//! The tree predates §17 and does not yet satisfy the layering. The
//! [`GRANDFATHERED`] list pins every offending edge that exists *today*;
//! each is a tracked defect scheduled for the §17 burn-down (`PLAN.md`).
//! The list is append-never: it may only shrink, and a *new* violating
//! edge is always rejected. The transitive non-GUI → GUI rule has no
//! exceptions — the desktop boundary is clean and must stay clean.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The stratum a crate belongs to, derived from its workspace-relative
/// directory. The strata mirror the rows of the §17.4 graph.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Layer {
    Lib,
    ArchApi,
    ArchImpl,
    SchedApi,
    SchedImpl,
    KernelSubsystem,
    KernelCore,
    Driver,
    Userland,
    UserGui,
    /// Tooling and integration tests: outside the product layering.
    Tooling,
}

impl Layer {
    fn name(self) -> &'static str {
        match self {
            Layer::Lib => "lib",
            Layer::ArchApi => "kernel/arch/api",
            Layer::ArchImpl => "kernel/arch/<target>",
            Layer::SchedApi => "kernel/sched/api",
            Layer::SchedImpl => "kernel/sched/<impl>",
            Layer::KernelSubsystem => "kernel subsystem",
            Layer::KernelCore => "kernel/core",
            Layer::Driver => "drivers/*",
            Layer::Userland => "userland/*",
            Layer::UserGui => "userland/gui/*",
            Layer::Tooling => "tooling/tests",
        }
    }
}

/// A workspace member crate.
#[derive(Debug, Clone)]
pub struct Crate {
    pub name: String,
    pub rel_dir: String,
    pub layer: Layer,
    /// Names of in-workspace build-graph dependencies (no dev-deps).
    pub deps: Vec<String>,
}

/// A grandfathered edge, pinned to today's tree.
struct GrandfatheredEdge {
    from: &'static str,
    to: &'static str,
}

/// Edges that violate §17.4 / the concrete-scheduler rule *today*. Each
/// is a tracked defect for the §17 burn-down (`PLAN.md`); this list may
/// only shrink.
const GRANDFATHERED: &[GrandfatheredEdge] = &[
    // `kernel/virtio` still reaches down into the virtio *bus driver*
    // (`drivers/bus/virtio`) for the concrete PCI/MMIO transports its
    // kernel-side host drives. That edge is a separate, larger thread of
    // the §17 burn-down. The former `kernel/virtio -> userland/drvhost`
    // edge is resolved: the `VirtioHostFactory` seam now lives in
    // `lib/virtio`, so both sides depend on `lib/*` instead of each other.
    edge("rustos-kernel-virtio", "rustos-drv-bus-virtio"),
    // Architecture ports still name concrete kernel crates instead of
    // the Arch HAL `kernel/arch/api`. x86_64 has been migrated: it now
    // implements `rustos_arch_api::SchedulerArch` and no longer names a
    // scheduler crate. riscv64 remains grandfathered because its boot
    // pipeline (`boot.rs`) builds a `kernel_core::BootInfo` from
    // `kernel/{mem,sec}` types, names `SchedulerConfig` from the
    // scheduler API, and calls `kernel_main` directly; removing those
    // edges requires relocating the boot orchestration into `kernel/core`
    // (the single §17.4 selection point) and is tracked in the §17
    // burn-down (`PLAN.md`). The `kernel/sync` edge is resolved — those
    // primitives now live in `lib/sync`, which an `ArchImpl` may name.
    edge("rustos-arch-riscv64", "rustos-kernel-core"),
    edge("rustos-arch-riscv64", "rustos-kernel-sched-api"),
    edge("rustos-arch-riscv64", "rustos-kernel-mem"),
    edge("rustos-arch-riscv64", "rustos-kernel-sec"),
    edge("rustos-arch-riscv64", "rustos-kernel-irq"),
    // The x86_64 production kernel binary is a second integration point
    // beside `kernel/core`: it brings the allocator, the arch port, and
    // the boot-time drivers together. §17.4 allows exactly one selection
    // point (`kernel/core`); collapsing the two is part of the burn-down.
    edge("rustos-kernel", "rustos-kernel-core"),
    edge("rustos-kernel", "rustos-arch-x86_64"),
    edge("rustos-kernel", "rustos-drvhost"),
    edge("rustos-kernel", "rustos-drv-bus-virtio"),
];

const fn edge(from: &'static str, to: &'static str) -> GrandfatheredEdge {
    GrandfatheredEdge { from, to }
}

/// Classify a crate by its workspace-relative directory.
pub fn classify(rel_dir: &str) -> Layer {
    if rel_dir.starts_with("lib/") {
        Layer::Lib
    } else if rel_dir == "kernel/core" {
        Layer::KernelCore
    } else if rel_dir == "kernel/arch/api" {
        Layer::ArchApi
    } else if rel_dir.starts_with("kernel/arch/") {
        Layer::ArchImpl
    } else if rel_dir == "kernel/sched/api" {
        Layer::SchedApi
    } else if rel_dir == "kernel/sched" || rel_dir.starts_with("kernel/sched/") {
        Layer::SchedImpl
    } else if rel_dir.starts_with("kernel/") {
        Layer::KernelSubsystem
    } else if rel_dir.starts_with("drivers/") {
        Layer::Driver
    } else if rel_dir.starts_with("userland/gui/") {
        Layer::UserGui
    } else if rel_dir.starts_with("userland/") {
        Layer::Userland
    } else {
        Layer::Tooling
    }
}

/// The §17.4 layering matrix: may a crate in `from` depend on a crate in
/// `to`? `Tooling` (tools/tests) is exempt and never a source here.
pub fn layer_allows(from: Layer, to: Layer) -> bool {
    use Layer::{
        ArchApi, ArchImpl, Driver, KernelCore, KernelSubsystem, Lib, SchedApi, SchedImpl, Tooling,
        UserGui, Userland,
    };
    match from {
        // Leaf strata that may consume only shared libraries: `lib/*`
        // itself, the Arch HAL surface, drivers, and non-GUI userland.
        Lib | ArchApi | Driver | Userland => matches!(to, Lib),
        // The architecture port and the scheduler API both sit directly
        // above the Arch HAL.
        ArchImpl | SchedApi => matches!(to, ArchApi | Lib),
        SchedImpl => matches!(to, SchedApi | ArchApi | Lib),
        KernelSubsystem => matches!(to, KernelSubsystem | ArchApi | SchedApi | Lib),
        // The single selection point: it may name every kernel stratum.
        KernelCore => matches!(
            to,
            KernelCore | KernelSubsystem | ArchApi | ArchImpl | SchedApi | SchedImpl | Lib
        ),
        // GUI crates compose with each other and `lib/*` only.
        UserGui => matches!(to, Lib | UserGui),
        // Tooling and tests sit outside the product layering.
        Tooling => true,
    }
}

/// True when a kernel crate at `rel_dir` is permitted to name a concrete
/// scheduler crate: only `kernel/core` and `kernel/sched/*` (§17.1).
fn may_name_concrete_scheduler(rel_dir: &str) -> bool {
    rel_dir == "kernel/core" || rel_dir == "kernel/sched" || rel_dir.starts_with("kernel/sched/")
}

fn is_grandfathered(from: &str, to: &str) -> bool {
    GRANDFATHERED.iter().any(|e| e.from == from && e.to == to)
}

/// Build the crate graph from the workspace manifests under `root`.
pub fn build_graph(root: &Path) -> Result<Vec<Crate>, String> {
    let members = workspace_members(root)?;
    // Map directory → crate name so path deps resolve to names.
    let mut by_dir: BTreeMap<String, String> = BTreeMap::new();
    let mut parsed: Vec<(String, String, Vec<String>)> = Vec::new();
    for rel_dir in &members {
        let manifest = root.join(rel_dir).join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("deps-check: cannot read {}: {e}", manifest.display()))?;
        let name = package_name(&text)
            .ok_or_else(|| format!("deps-check: no [package] name in {}", manifest.display()))?;
        let dep_dirs = dependency_dirs(&text, rel_dir);
        by_dir.insert(rel_dir.clone(), name.clone());
        parsed.push((rel_dir.clone(), name, dep_dirs));
    }

    let mut crates = Vec::with_capacity(parsed.len());
    for (rel_dir, name, dep_dirs) in parsed {
        let mut deps = Vec::new();
        for d in dep_dirs {
            if let Some(dep_name) = by_dir.get(&d) {
                if *dep_name != name {
                    deps.push(dep_name.clone());
                }
            }
        }
        deps.sort();
        deps.dedup();
        let layer = classify(&rel_dir);
        crates.push(Crate {
            name,
            rel_dir,
            layer,
            deps,
        });
    }
    Ok(crates)
}

/// Run all §17 dependency checks against the workspace at `root`.
pub fn run(root: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let crates = build_graph(root)?;
    let violations = analyze(&crates);
    if violations.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("deps-check: modularity violations (AGENTS.md §17.4 / §17.5):\n");
    for v in &violations {
        let _ = writeln!(msg, "  {v}");
    }
    Err(msg)
}

/// Compute the full set of violation messages for a crate graph.
pub fn analyze(crates: &[Crate]) -> Vec<String> {
    let by_name: BTreeMap<&str, &Crate> = crates.iter().map(|c| (c.name.as_str(), c)).collect();
    let mut violations = Vec::new();

    for c in crates {
        for dep in &c.deps {
            let Some(target) = by_name.get(dep.as_str()) else {
                continue;
            };
            if is_grandfathered(&c.name, dep) {
                continue;
            }
            // Concrete-scheduler-naming rule (§17.1 / §17.5).
            if target.layer == Layer::SchedImpl
                && c.rel_dir.starts_with("kernel/")
                && !may_name_concrete_scheduler(&c.rel_dir)
            {
                violations.push(format!(
                    "{} ({}) names concrete scheduler crate {} ({}); only \
                     kernel/core and kernel/sched/* may (§17.1)",
                    c.name, c.rel_dir, target.name, target.rel_dir,
                ));
                continue;
            }
            if !layer_allows(c.layer, target.layer) {
                violations.push(format!(
                    "{} [{}] must not depend on {} [{}] (§17.4)",
                    c.name,
                    c.layer.name(),
                    target.name,
                    target.layer.name(),
                ));
            }
        }
    }

    // Transitive non-GUI → GUI rule (§17.3 / §17.5). No exceptions.
    for c in crates {
        if matches!(c.layer, Layer::UserGui | Layer::Tooling) {
            continue;
        }
        if let Some(path) = reaches_gui(c, &by_name) {
            violations.push(format!(
                "non-GUI crate {} [{}] transitively depends on userland/gui/* \
                 via {} (§17.3)",
                c.name,
                c.layer.name(),
                path.join(" -> "),
            ));
        }
    }

    violations.sort();
    violations.dedup();
    violations
}

/// Return a dependency path from `start` to a `userland/gui/*` crate, or
/// `None` if the desktop is unreachable.
fn reaches_gui(start: &Crate, by_name: &BTreeMap<&str, &Crate>) -> Option<Vec<String>> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<Vec<String>> = vec![vec![start.name.clone()]];
    while let Some(path) = stack.pop() {
        let current = path.last().expect("non-empty path");
        let Some(node) = by_name.get(current.as_str()) else {
            continue;
        };
        if !seen.insert(node.name.as_str()) {
            continue;
        }
        for dep in &node.deps {
            if let Some(target) = by_name.get(dep.as_str()) {
                if target.layer == Layer::UserGui {
                    let mut found = path.clone();
                    found.push(dep.clone());
                    return Some(found);
                }
                let mut next = path.clone();
                next.push(dep.clone());
                stack.push(next);
            }
        }
    }
    None
}

/// Parse the `members = [ ... ]` array from the workspace manifest.
///
/// The scan is line-based and strips `#` comments first. This matters
/// twice: a member entry shares a comma-delimited chunk with the comment
/// above it (so a comma split would miss the first entry of each block),
/// and the comments themselves contain stray brackets (e.g. ``[lib]`` /
/// ``[[bin]]``) that would otherwise be mistaken for the array's close.
fn workspace_members(root: &Path) -> Result<Vec<String>, String> {
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("deps-check: cannot read {}: {e}", manifest.display()))?;

    let mut members = Vec::new();
    let mut in_members = false;
    let mut found_array = false;
    for line in text.lines() {
        let code = line.split('#').next().unwrap_or("");
        if !in_members {
            let Some(eq) = code.find('=') else { continue };
            if code[..eq].trim() != "members" {
                continue;
            }
            let Some(open) = code.find('[') else { continue };
            in_members = true;
            found_array = true;
            push_member(&code[open + 1..], &mut members);
            if code[open + 1..].contains(']') {
                return Ok(members);
            }
            continue;
        }
        if let Some(close) = code.find(']') {
            push_member(&code[..close], &mut members);
            return Ok(members);
        }
        push_member(code, &mut members);
    }
    if found_array {
        Err("deps-check: unterminated workspace `members` array".to_string())
    } else {
        Err("deps-check: no workspace `members` array".to_string())
    }
}

/// Extract a quoted member path from a comment-stripped code fragment.
fn push_member(fragment: &str, members: &mut Vec<String>) {
    let trimmed = fragment.trim().trim_end_matches(',').trim();
    if let Some(value) = string_literal(trimmed) {
        members.push(value);
    }
}

/// Extract the `[package] name = "..."` value.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = trimmed.strip_prefix("name") {
                let rest = rest.trim_start().strip_prefix('=')?.trim();
                return string_literal(rest);
            }
        }
    }
    None
}

/// Resolve every build-graph `path =` dependency in `manifest` to a
/// workspace-relative directory. `[dev-dependencies]` tables are skipped.
fn dependency_dirs(manifest: &str, crate_rel_dir: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut in_dep_section = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dep_section = is_build_dependency_header(trimmed);
            continue;
        }
        if !in_dep_section {
            continue;
        }
        if let Some(path) = extract_path_value(trimmed) {
            if let Some(dir) = normalize_join(crate_rel_dir, &path) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// True for `[dependencies]`, `[build-dependencies]`, and their
/// `[target.'...'.(build-)dependencies]` / sub-table forms, but never for
/// any `dev-dependencies` table.
fn is_build_dependency_header(header: &str) -> bool {
    let inner = header.trim_start_matches('[').trim_end_matches(']').trim();
    if inner.contains("dev-dependencies") {
        return false;
    }
    inner.ends_with("dependencies") || inner.contains("dependencies.")
}

/// Extract the value of a `path = "..."` key if present on the line.
fn extract_path_value(line: &str) -> Option<String> {
    let idx = line.find("path")?;
    let after = line[idx + "path".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    string_literal(after)
}

/// Parse a leading `"..."` string literal, ignoring any trailing tokens.
fn string_literal(s: &str) -> Option<String> {
    let s = s.trim();
    let rest = s.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Join `base` (a workspace-relative dir) with a relative `path` and
/// normalize `.`/`..` segments into a clean `/`-separated dir.
fn normalize_join(base: &str, path: &str) -> Option<String> {
    let mut segments: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    }

    #[test]
    fn workspace_is_clean_modulo_grandfathered() {
        let root = workspace_root();
        let crates = build_graph(&root).expect("graph");
        let violations = analyze(&crates);
        assert!(
            violations.is_empty(),
            "unexpected §17 violations: {violations:#?}"
        );
    }

    #[test]
    fn drvhost_has_no_production_edge_to_virtio_bus() {
        // Burn-down regression: `userland/system/drvhost` reaches the
        // virtio bus crate only from its `[dev-dependencies]` (the
        // integration-test fixtures), never from production code, so the
        // §17.4 `Userland -> Driver` edge does not exist in the build
        // graph and is no longer grandfathered. A future *production*
        // dependency must be rejected, not silently tolerated.
        let root = workspace_root();
        let crates = build_graph(&root).expect("graph");
        let drvhost = crates
            .iter()
            .find(|c| c.name == "rustos-drvhost")
            .expect("drvhost present");
        assert!(
            !drvhost.deps.iter().any(|d| d == "rustos-drv-bus-virtio"),
            "drvhost gained a production dependency on the virtio bus crate"
        );
        assert!(
            !is_grandfathered("rustos-drvhost", "rustos-drv-bus-virtio"),
            "stale grandfather entry must stay removed"
        );
    }

    #[test]
    fn kernel_virtio_has_no_edge_to_drvhost() {
        // Burn-down regression (§17.4): the `VirtioHostFactory` seam was
        // hoisted into `lib/virtio`, so the kernel-side factory crate
        // (`kernel/virtio`) and the userland driver host (`drvhost`) both
        // depend on `lib/*` instead of on each other. The former
        // `kernel/virtio -> userland/drvhost` edge (a `KernelSubsystem ->
        // Userland` inversion) must stay gone, not be re-grandfathered.
        let root = workspace_root();
        let crates = build_graph(&root).expect("graph");
        let kernel_virtio = crates
            .iter()
            .find(|c| c.name == "rustos-kernel-virtio")
            .expect("kernel-virtio present");
        assert!(
            !kernel_virtio.deps.iter().any(|d| d == "rustos-drvhost"),
            "kernel/virtio regained a dependency on userland/drvhost"
        );
        assert!(
            !is_grandfathered("rustos-kernel-virtio", "rustos-drvhost"),
            "stale grandfather entry for kernel/virtio -> drvhost must stay removed"
        );
        assert!(
            kernel_virtio.deps.iter().any(|d| d == "rustos-virtio"),
            "kernel/virtio must consume the VirtioHostFactory seam from lib/virtio"
        );
    }

    #[test]
    fn virtio_driver_layer_is_on_lib_only() {
        // §17 burn-down regression (scope C): the bus-agnostic virtio
        // protocol now lives in `lib/virtio`, so the virtio bus driver
        // and the virtio device drivers depend on `lib/*` only — the bus
        // crate no longer links `kernel/{mem,sec,irq}` (the kernel host
        // moved to `kernel/virtio`), and the device drivers no longer
        // depend on the bus driver crate (`AGENTS.md` §17.4). These edges
        // must stay removed, not silently re-grandfathered.
        let root = workspace_root();
        let crates = build_graph(&root).expect("graph");
        let dep_of = |name: &str| -> Vec<String> {
            crates
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} present"))
                .deps
                .clone()
        };

        let bus = dep_of("rustos-drv-bus-virtio");
        for kernel_crate in [
            "rustos-kernel-mem",
            "rustos-kernel-sec",
            "rustos-kernel-irq",
        ] {
            assert!(
                !bus.iter().any(|d| d == kernel_crate),
                "virtio bus driver regained a kernel dependency on {kernel_crate}"
            );
            assert!(
                !is_grandfathered("rustos-drv-bus-virtio", kernel_crate),
                "stale grandfather entry for the virtio bus driver must stay removed"
            );
        }
        assert!(
            bus.iter().any(|d| d == "rustos-virtio"),
            "virtio bus driver must consume the protocol from lib/virtio"
        );

        for (driver, expected_lib) in [
            ("rustos-drv-storage-virtio-blk", "rustos-virtio"),
            ("rustos-drv-network-virtio-net", "rustos-virtio"),
        ] {
            let deps = dep_of(driver);
            assert!(
                !deps.iter().any(|d| d == "rustos-drv-bus-virtio"),
                "{driver} regained a direct dependency on the virtio bus driver"
            );
            assert!(
                !is_grandfathered(driver, "rustos-drv-bus-virtio"),
                "stale grandfather entry for {driver} must stay removed"
            );
            assert!(
                deps.iter().any(|d| d == expected_lib),
                "{driver} must consume the virtio protocol from {expected_lib}"
            );
        }
    }

    #[test]
    fn graph_resolves_known_edges() {
        let root = workspace_root();
        let crates = build_graph(&root).expect("graph");
        let core = crates
            .iter()
            .find(|c| c.name == "rustos-kernel-core")
            .expect("core present");
        assert_eq!(core.layer, Layer::KernelCore);
        assert!(core.deps.iter().any(|d| d == "rustos-kernel-mem"));
        // dev-dependency self-reference must not appear as an edge.
        assert!(!core.deps.iter().any(|d| d == "rustos-kernel-core"));
    }

    #[test]
    fn first_member_after_a_comment_is_parsed() {
        // Regression: members and their preceding `#` comment share a
        // comma-delimited chunk, so a naive comma split dropped the first
        // entry of every commented block (e.g. `lib/abi`, `kernel/core`).
        let members = workspace_members(&workspace_root()).expect("members");
        for required in [
            "kernel/core",
            "lib/abi",
            "drivers/display/vesa",
            "userland/system/drvhost",
        ] {
            assert!(
                members.iter().any(|m| m == required),
                "missing member {required}; parsed: {members:#?}"
            );
        }
    }

    #[test]
    fn classify_matches_strata() {
        assert_eq!(classify("lib/abi"), Layer::Lib);
        assert_eq!(classify("kernel/core"), Layer::KernelCore);
        assert_eq!(classify("kernel/arch/x86_64"), Layer::ArchImpl);
        assert_eq!(classify("kernel/arch/api"), Layer::ArchApi);
        assert_eq!(classify("kernel/sched"), Layer::SchedImpl);
        assert_eq!(classify("kernel/mem"), Layer::KernelSubsystem);
        assert_eq!(classify("drivers/bus/pci"), Layer::Driver);
        assert_eq!(classify("userland/gui/wm"), Layer::UserGui);
        assert_eq!(classify("userland/system/init"), Layer::Userland);
        assert_eq!(classify("tools/xtask"), Layer::Tooling);
    }

    #[test]
    fn lib_must_not_depend_on_kernel() {
        assert!(layer_allows(Layer::Lib, Layer::Lib));
        assert!(!layer_allows(Layer::Lib, Layer::KernelSubsystem));
        assert!(!layer_allows(Layer::Driver, Layer::KernelSubsystem));
        assert!(!layer_allows(Layer::ArchImpl, Layer::SchedImpl));
    }

    #[test]
    fn synthetic_gui_dependency_is_flagged() {
        let crates = vec![
            Crate {
                name: "rustos-init".into(),
                rel_dir: "userland/system/init".into(),
                layer: Layer::Userland,
                deps: vec!["rustos-wm".into()],
            },
            Crate {
                name: "rustos-wm".into(),
                rel_dir: "userland/gui/wm".into(),
                layer: Layer::UserGui,
                deps: vec![],
            },
        ];
        let violations = analyze(&crates);
        assert!(
            violations.iter().any(|v| v.contains("userland/gui")),
            "{violations:#?}"
        );
    }

    #[test]
    fn synthetic_concrete_scheduler_naming_is_flagged() {
        let crates = vec![
            Crate {
                name: "rustos-kernel-mem".into(),
                rel_dir: "kernel/mem".into(),
                layer: Layer::KernelSubsystem,
                deps: vec!["rustos-kernel-eevdf".into()],
            },
            Crate {
                name: "rustos-kernel-eevdf".into(),
                rel_dir: "kernel/sched/eevdf".into(),
                layer: Layer::SchedImpl,
                deps: vec![],
            },
        ];
        let violations = analyze(&crates);
        assert!(
            violations.iter().any(|v| v.contains("concrete scheduler")),
            "{violations:#?}"
        );
    }

    #[test]
    fn normalize_join_resolves_parents() {
        assert_eq!(
            normalize_join("drivers/bus/pci", "../../../lib/abi").as_deref(),
            Some("lib/abi")
        );
        assert_eq!(
            normalize_join("kernel/core", "../mem").as_deref(),
            Some("kernel/mem")
        );
    }
}
