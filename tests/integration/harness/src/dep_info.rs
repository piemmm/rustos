//! Dep-info-driven `cargo:rerun-if-changed` emission for build scripts that
//! run an inner `cargo build`.
//!
//! A fixture build script that cross-compiles a guest program (a driver
//! bundle, an app store binary) and embeds the result must rebuild whenever
//! **any** source the inner build consumed changes — including transitive
//! `lib/*` dependencies the script cannot reasonably enumerate by hand. A
//! hand-maintained `rerun-if-changed` list that names only the program's own
//! `src/main.rs` silently ships a stale embedded binary the moment a shared
//! library it links is edited: the outer fixture keeps its cached bytes while
//! the tree has moved on, and the QEMU vertical then exercises code that no
//! longer exists.
//!
//! The inner build already knows its exact input set: rustc writes a
//! makefile-style dep-info file (`<package>.d`) beside the linked binary,
//! listing every source file the compilation read.
//! [`emit_dep_info_reruns`](crate::dep_info::emit_dep_info_reruns) parses
//! that file and registers each prerequisite with cargo, so the fixture's
//! freshness is derived from the compiler's own record rather than a
//! hand-kept list that rots.

use std::fs;
use std::path::{Path, PathBuf};

/// Parse the prerequisite paths out of makefile-style dep-info `contents`.
///
/// A dep-info file holds one or more rules of the form
/// `target: prereq prereq …`; prerequisites are separated by spaces, a space
/// *inside* a path is escaped as `\ `, and a trailing `\` continues the rule
/// on the next line. Rules with no prerequisites (the per-file stub rules
/// rustc appends) contribute nothing. Duplicate paths across rules are
/// collapsed, preserving first-seen order, so the emitter prints each input
/// once.
#[must_use]
pub fn parse_dep_info(contents: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in contents.lines() {
        // Only rule lines carry prerequisites; the text after the first
        // unescaped `:` is the prerequisite list. A Windows drive letter
        // (`C:\…`) cannot appear as the *rule* separator here because rustc
        // writes the target first, so splitting on the first `: ` (colon
        // followed by whitespace) is unambiguous; a rule with an empty
        // prerequisite list ends at the colon.
        let Some(colon) = line.find(": ") else {
            continue;
        };
        let prereqs = &line[colon + 2..];
        let mut current = String::new();
        let mut chars = prereqs.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    // An escaped space is part of the path.
                    chars.next();
                    current.push(' ');
                }
                '\\' if chars.peek().is_none() => {
                    // A trailing backslash is a line continuation, not a
                    // path byte.
                }
                ' ' => {
                    push_unique(&mut seen, &mut current);
                }
                _ => current.push(c),
            }
        }
        push_unique(&mut seen, &mut current);
    }
    seen
}

/// Move a completed path token into `seen` unless it is empty or already
/// recorded.
fn push_unique(seen: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        if !seen.iter().any(|p| p == current.as_str()) {
            seen.push(current.clone());
        }
        current.clear();
    }
}

/// Read the dep-info file at `dep_info` and emit one
/// `cargo:rerun-if-changed` line per recorded source file, resolving
/// relative paths against `build_cwd` (the directory the inner `cargo
/// build` ran in).
///
/// Call this from a build script immediately after the inner build
/// succeeds, pointing at the `<target-dir>/<triple>/debug/<package>.d`
/// rustc wrote beside the linked binary.
///
/// # Panics
///
/// Panics if the dep-info file cannot be read: the inner build just
/// produced it, so an unreadable file means the caller named the wrong
/// path, and registering *no* inputs would silently freeze the fixture —
/// fail loud instead.
pub fn emit_dep_info_reruns(dep_info: &Path, build_cwd: &Path) {
    let contents = fs::read_to_string(dep_info)
        .unwrap_or_else(|e| panic!("read dep-info {}: {e}", dep_info.display()));
    for prereq in parse_dep_info(&contents) {
        let path = PathBuf::from(&prereq);
        let absolute = if path.is_absolute() {
            path
        } else {
            build_cwd.join(path)
        };
        println!("cargo:rerun-if-changed={}", absolute.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_rule() {
        let deps = parse_dep_info("out/bin: src/main.rs lib/a.rs\n");
        assert_eq!(deps, ["src/main.rs", "lib/a.rs"]);
    }

    #[test]
    fn unescapes_spaces_inside_paths() {
        let deps = parse_dep_info("out/bin: src/my\\ file.rs other.rs\n");
        assert_eq!(deps, ["src/my file.rs", "other.rs"]);
    }

    #[test]
    fn collapses_duplicates_across_rules() {
        let deps = parse_dep_info(
            "out/bin: src/main.rs shared.rs\nout/bin.d: src/main.rs shared.rs extra.rs\n",
        );
        assert_eq!(deps, ["src/main.rs", "shared.rs", "extra.rs"]);
    }

    #[test]
    fn ignores_stub_rules_and_blank_lines() {
        let deps = parse_dep_info("src/main.rs:\n\nout/bin: src/main.rs\n");
        assert_eq!(deps, ["src/main.rs"]);
    }

    #[test]
    fn drops_a_trailing_continuation_backslash() {
        let deps = parse_dep_info("out/bin: first.rs \\\n");
        assert_eq!(deps, ["first.rs"]);
    }

    #[test]
    fn empty_input_yields_no_paths() {
        assert!(parse_dep_info("").is_empty());
    }
}
