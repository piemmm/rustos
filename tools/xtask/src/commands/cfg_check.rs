//! `cargo xtask cfg-check` implementation (`AGENTS.md` §17.2 / §17.5).
//!
//! §17.2 forbids target-conditional compilation —
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

use std::path::Path;

/// Directory prefixes (workspace-relative, `/`-separated) where
/// target-conditional compilation is permitted by §17.2.
const ALLOWED: &[&str] = &["kernel/arch/", ".cargo/", "tools/mkimage/", "tools/xtask/"];

/// Directory prefixes that violate §17.2 *today* and are tolerated until
/// the §17 burn-down lands (`PLAN.md`). This list is append-never: it may
/// only shrink. Each entry is a tracked defect, not a sanctioned pattern.
///
/// Empty: every directory that named the target instruction set inline has
/// been migrated. `kernel/rustos-kernel` was the last entry; it now gates
/// its freestanding body on the build-script-emitted `freestanding` cfg
/// (`kernel/rustos-kernel/build.rs`) instead of `cfg(target_arch = …)`.
const GRANDFATHERED: &[&str] = &[];

/// The cfg predicates §17.2 forbids outside the allow-list.
const FORBIDDEN_KEYS: &[&str] = &["target_arch", "target_pointer_width"];

/// A single offending occurrence: a workspace-relative path and the
/// 1-based line number that names a forbidden predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Scan the workspace rooted at `root` and return every §17.2 violation
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
                if is_allowed(&rel) {
                    continue;
                }
                scan_file(&path, &rel, &mut out)?;
            }
        }
    }
    out.retain(|v| !is_grandfathered(&v.path));
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn scan_file(path: &Path, rel: &str, out: &mut Vec<Violation>) -> Result<(), String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("cfg-check: cannot read {}: {e}", path.display()))?;
    for (idx, line) in src.lines().enumerate() {
        if line_offends(line) {
            out.push(Violation {
                path: rel.to_string(),
                line: idx + 1,
                text: line.trim().to_string(),
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

fn is_allowed(rel: &str) -> bool {
    ALLOWED.iter().any(|p| rel.starts_with(p))
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
    let mut msg = String::from(
        "cfg-check: target-conditional compilation is forbidden outside \
         the architecture ports and build glue (AGENTS.md §17.2):\n",
    );
    for v in &violations {
        let _ = writeln!(msg, "  {}:{}: {}", v.path, v.line, v.text);
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
}
