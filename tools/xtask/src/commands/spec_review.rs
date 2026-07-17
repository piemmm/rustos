//! `cargo xtask spec-review` implementation.
//!
//! lets AI assistance *draft* specifications, proofs, models, and fuzz
//! harnesses, but the verifier — not the model — is the only oracle. A draft
//! that has not yet been reviewed by a human under the senior-developer
//! bar must carry a marker, and this checker "fails CI if any such marker
//! reaches `main`". It is the gate that keeps an unreviewed, AI-drafted
//! load-bearing artefact out of the trunk.
//!
//! The checker walks every tracked `.rs` source file and fails if the draft
//! marker appears anywhere. Its own source is skipped (it necessarily names
//! the marker to search for it); documentation is `.md`, not scanned, so the
//! charter prose that defines the marker does not trip the gate.

use std::path::Path;

/// The distinctive token of the draft marker (`// SPEC-DRAFT:`). Matching the
/// bare token rather than the exact comment punctuation catches every spelling
/// (`//!`, `///`, trailing text) without a brittle regex.
const MARKER: &str = "SPEC-DRAFT";

/// The checker's own file, skipped because it necessarily contains [`MARKER`].
const SELF_FILE: &str = "spec_review.rs";

/// A single offending occurrence: a workspace-relative path and the 1-based
/// line number that carries an unreviewed draft marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Scan the workspace rooted at `root` and return every draft marker found.
pub fn scan(root: &Path) -> Result<Vec<Violation>, String> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("spec-review: cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("spec-review: dir entry: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("spec-review: file type {}: {e}", path.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if name == "target" || name == ".git" {
                    continue;
                }
                dirs.push(path);
            } else if file_type.is_file() && name.ends_with(".rs") && name != SELF_FILE {
                scan_file(&path, &relative(root, &path), &mut out)?;
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn scan_file(path: &Path, rel: &str, out: &mut Vec<Violation>) -> Result<(), String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("spec-review: cannot read {}: {e}", path.display()))?;
    for (idx, line) in src.lines().enumerate() {
        if line.contains(MARKER) {
            out.push(Violation {
                path: rel.to_string(),
                line: idx + 1,
                text: line.trim().to_string(),
            });
        }
    }
    Ok(())
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Run the check, printing a report and returning an error if any draft
/// marker remains in the tree.
pub fn run(root: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let violations = scan(root)?;
    if violations.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "spec-review: unreviewed draft markers must not reach `main` \
         (AGENTS.md §19.7); review the artefact under the §2.6 bar and \
         remove the marker:\n",
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
    fn workspace_carries_no_draft_markers() {
        let root = workspace_root();
        let violations = scan(&root).expect("scan");
        assert!(
            violations.is_empty(),
            "unexpected §19.7 draft markers: {violations:#?}"
        );
    }

    #[test]
    fn marker_is_detected_in_every_comment_spelling() {
        let sample = format!("    // {MARKER}: drafted by an assistant, awaiting review");
        let mut out = Vec::new();
        let dir = std::env::temp_dir().join(format!("tairix-spec-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("draft.rs");
        std::fs::write(&file, format!("fn f() {{}}\n{sample}\n")).expect("write");
        scan_file(&file, "draft.rs", &mut out).expect("scan file");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 2);
    }

    #[test]
    fn clean_source_is_accepted() {
        let mut out = Vec::new();
        let dir = std::env::temp_dir().join(format!("tairix-spec-clean-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("clean.rs");
        std::fs::write(&file, "fn ok() {}\n// a normal comment\n").expect("write");
        scan_file(&file, "clean.rs", &mut out).expect("scan file");
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.is_empty());
    }
}
