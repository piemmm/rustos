//! `cargo xtask cfg-check` implementation.
//!
//! the charter forbids target-conditional compilation —
//! `#[cfg(target_arch = "…")]`, `#[cfg(target_pointer_width = …)]`, and
//! equivalents — everywhere except the architecture ports
//! (`kernel/arch/<target>/`) and the build glue (`.cargo/`,
//! `tools/mkimage/`, `tools/xtask/`). Conditioning behaviour on the
//! target anywhere else means the modularity boundary (the Arch HAL) has
//! leaked, so it is a defect.
//!
//! This checker walks every tracked `.rs` source file in the workspace
//! and fails if a `cfg`/`cfg_attr` predicate names `target_arch` or
//! `target_pointer_width` outside the allow-list. A small, explicit
//! [`GRANDFATHERED`] list pins the directories that violate the rule
//! *today*; each is a tracked defect to be burned down (see `PLAN.md`),
//! and the set may only shrink — a new file under a grandfathered tree
//! is still rejected unless the tree itself is listed.
//!
//! Inside a freestanding port the allow-list stops applying and a second
//! rule takes over: a `cfg` naming `target_arch` must also name
//! `target_os`. Gating on the architecture alone selects the bare-metal
//! body in a *host* build of the port too — where the instruction is
//! privileged, and where the UB oracle cannot interpret it at all — so
//! the omission only shows up on the machine whose architecture the port
//! names, and passes everywhere else.

use std::path::Path;

/// Directory prefixes (workspace-relative, `/`-separated) where
/// target-conditional compilation is permitted by.
const ALLOWED: &[&str] = &["kernel/arch/", ".cargo/", "tools/mkimage/", "tools/xtask/"];

/// Directory prefixes that violate *today* and are tolerated until
/// the burn-down lands (`PLAN.md`). This list is append-never: it may
/// only shrink. Each entry is a tracked defect, not a sanctioned pattern.
///
/// Empty: every directory that named the target instruction set inline has
/// been migrated. `kernel/tairix-kernel` was the last entry; it now gates
/// its freestanding body on the build-script-emitted `freestanding` cfg
/// (`kernel/tairix-kernel/build.rs`) instead of `cfg(target_arch = …)`.
const GRANDFATHERED: &[&str] = &[];

/// The cfg predicates the charter forbids outside the allow-list.
const FORBIDDEN_KEYS: &[&str] = &["target_arch", "target_pointer_width"];

/// The ports whose target is freestanding, where an architecture gate
/// must also name `target_os`.
///
/// `kernel/arch/wasm32` is absent deliberately: its target reports
/// `target_os = "unknown"`, so pairing the gate there would disable the
/// real body rather than the host one.
const FREESTANDING_PORTS: &[&str] = &[
    "kernel/arch/x86_64/",
    "kernel/arch/aarch64/",
    "kernel/arch/riscv64/",
];

/// Which rule an occurrence breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Target-conditional compilation outside the ports and build glue.
    TargetConditional,
    /// A freestanding port's architecture gate that omits `target_os`,
    /// so it also selects the bare-metal body in a host build.
    ArchGateWithoutOs,
}

/// A single offending occurrence: a workspace-relative path and the
/// 1-based line number that names a forbidden predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub line: usize,
    pub text: String,
    pub rule: Rule,
}

/// Scan the workspace rooted at `root` and return every violation
/// outside the allow-list and grandfather list.
pub fn scan(root: &Path) -> Result<Vec<Violation>, String> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("cfg-check: cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cfg-check: dir entry: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("cfg-check: file type {}: {e}", path.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if name == "target" || name == ".git" {
                    continue;
                }
                dirs.push(path);
            } else if file_type.is_file() && name.ends_with(".rs") {
                let rel = relative(root, &path);
                let rule = if is_freestanding_port(&rel) {
                    Rule::ArchGateWithoutOs
                } else if is_allowed(&rel) {
                    continue;
                } else {
                    Rule::TargetConditional
                };
                scan_file(&path, &rel, rule, &mut out)?;
            }
        }
    }
    out.retain(|v| !is_grandfathered(&v.path));
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn scan_file(path: &Path, rel: &str, rule: Rule, out: &mut Vec<Violation>) -> Result<(), String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("cfg-check: cannot read {}: {e}", path.display()))?;
    for (idx, line) in src.lines().enumerate() {
        let offends = match rule {
            Rule::TargetConditional => line_offends(line),
            Rule::ArchGateWithoutOs => arch_gate_lacks_os(line),
        };
        if offends {
            out.push(Violation {
                path: rel.to_string(),
                line: idx + 1,
                text: line.trim().to_string(),
                rule,
            });
        }
    }
    Ok(())
}

/// A line offends when it mentions `cfg` and a forbidden predicate key.
/// Pairing the two keeps plain prose (a doc comment that merely names an
/// architecture) from tripping the check while still catching every
/// `cfg`/`cfg_attr`/`cfg!` form.
fn line_offends(line: &str) -> bool {
    line.contains("cfg") && FORBIDDEN_KEYS.iter().any(|k| line.contains(k))
}

/// Inside a freestanding port: a `cfg` gating on `target_arch` alone.
///
/// Comments are skipped — one gates no compilation, and a wrapped
/// sentence quoting a predicate would otherwise read as an offence.
/// Line-based like [`line_offends`], so a predicate split across lines
/// reads as unpaired; every gate in the ports fits on one line today.
fn arch_gate_lacks_os(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return false;
    }
    line.contains("cfg") && line.contains("target_arch") && !line.contains("target_os")
}

fn is_allowed(rel: &str) -> bool {
    ALLOWED.iter().any(|p| rel.starts_with(p))
}

fn is_freestanding_port(rel: &str) -> bool {
    FREESTANDING_PORTS.iter().any(|p| rel.starts_with(p))
}

fn is_grandfathered(rel: &str) -> bool {
    GRANDFATHERED.iter().any(|p| rel.starts_with(p))
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Run the check, printing a report and returning an error if any
/// non-grandfathered violation remains.
pub fn run(root: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let violations = scan(root)?;
    if violations.is_empty() {
        return Ok(());
    }
    let mut msg = String::new();
    for (rule, heading) in [
        (
            Rule::TargetConditional,
            "cfg-check: target-conditional compilation is forbidden outside \
             the architecture ports and build glue (AGENTS.md §17.2):",
        ),
        (
            Rule::ArchGateWithoutOs,
            "cfg-check: a freestanding port's `target_arch` gate must also name \
             `target_os` (AGENTS.md §17.2) — gating on the architecture alone \
             selects the bare-metal body in a host build of the port too:",
        ),
    ] {
        let mut hit = violations.iter().filter(|v| v.rule == rule).peekable();
        if hit.peek().is_none() {
            continue;
        }
        let _ = writeln!(msg, "{heading}");
        for v in hit {
            let _ = writeln!(msg, "  {}:{}: {}", v.path, v.line, v.text);
        }
    }
    Err(msg)
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
        let violations = scan(&root).expect("scan");
        assert!(
            violations.is_empty(),
            "unexpected §17.2 violations: {violations:#?}"
        );
    }

    #[test]
    fn arch_ports_are_allowed() {
        assert!(is_allowed("kernel/arch/x86_64/src/preempt.rs"));
        assert!(is_allowed("tools/xtask/src/commands/cfg_check.rs"));
        assert!(!is_allowed("kernel/mem/src/lib.rs"));
    }

    #[test]
    fn detects_cfg_target_arch_only_with_cfg() {
        assert!(line_offends("#[cfg(target_arch = \"x86_64\")]"));
        assert!(line_offends(
            "#![cfg_attr(target_pointer_width = \"64\", x)]"
        ));
        assert!(!line_offends("// runs on the x86_64 target_arch in prose"));
        assert!(!line_offends("#[cfg(target_os = \"none\")]"));
    }

    #[test]
    fn freestanding_ports_take_the_arch_gate_rule() {
        assert!(is_freestanding_port(
            "kernel/arch/riscv64/src/kernel_arch.rs"
        ));
        // Its target is `unknown`, not `none`, so the pairing does not apply.
        assert!(!is_freestanding_port("kernel/arch/wasm32/src/lib.rs"));
        // Arch-neutral, and not a port.
        assert!(!is_freestanding_port("kernel/arch/api/src/lib.rs"));
    }

    /// The exact shape that let a host build execute `rdtsc` and took the
    /// UB oracle's whole run down on an x86_64 runner while passing on
    /// every other host.
    #[test]
    fn an_arch_gate_without_target_os_is_caught() {
        assert!(arch_gate_lacks_os(
            "        #[cfg(target_arch = \"x86_64\")]"
        ));
        assert!(arch_gate_lacks_os("#[cfg(not(target_arch = \"riscv64\"))]"));
        assert!(!arch_gate_lacks_os(
            "#[cfg(all(target_arch = \"x86_64\", target_os = \"none\"))]"
        ));
        assert!(!arch_gate_lacks_os(
            "#[cfg(not(all(target_arch = \"aarch64\", target_os = \"none\")))]"
        ));
        // A gate on the OS alone is already host-safe.
        assert!(!arch_gate_lacks_os(
            "#[cfg(any(target_os = \"none\", doc))]"
        ));
    }

    /// A comment gates no compilation, and a wrapped sentence quoting a
    /// predicate must not read as an offence.
    #[test]
    fn a_comment_quoting_a_gate_is_not_an_offence() {
        assert!(!arch_gate_lacks_os("// the surrounding `cfg(target_arch ="));
        assert!(!arch_gate_lacks_os(
            "//! modules are gated on `cfg(target_arch = \"aarch64\")`"
        ));
        assert!(!arch_gate_lacks_os(
            "    /// Reads `0` unless `cfg(target_arch = \"riscv64\")`."
        ));
    }
}
