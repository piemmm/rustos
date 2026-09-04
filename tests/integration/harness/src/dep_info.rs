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
fn parse_dep_info(contents: &str) -> Vec<String> {
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

/// The full input set a dep-info record implies, in first-seen order.
///
/// Every prerequisite is resolved against `build_cwd` (the directory the
/// inner `cargo build` ran in) when it is relative, though cargo writes
/// absolute paths. Three rules turn the compiler's record into the set the
/// outer fixture must watch:
///
/// * prerequisites under `target_dir` are the inner build's own *outputs*,
///   and a caller may wipe that directory, so they are dropped — cargo
///   re-runs a build script on every invocation once a registered path is
///   missing;
/// * each recorded source's owning `Cargo.toml` joins the set, because rustc
///   records the files it *read* and a manifest edit changes what the guest
///   links without touching one of them;
/// * so does the workspace lockfile above `build_cwd`, which pins the
///   versions those manifests resolved to.
fn rerun_inputs(contents: &str, build_cwd: &Path, target_dir: &Path) -> Vec<PathBuf> {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !inputs.contains(&path) {
            inputs.push(path);
        }
    };
    for prereq in parse_dep_info(contents) {
        let path = PathBuf::from(&prereq);
        let source = if path.is_absolute() {
            path
        } else {
            build_cwd.join(path)
        };
        if source.starts_with(target_dir) {
            continue;
        }
        if let Some(manifest) = source
            .parent()
            .and_then(|d| nearest_ancestor(d, "Cargo.toml"))
        {
            push(manifest);
        }
        push(source);
    }
    if let Some(lockfile) = nearest_ancestor(build_cwd, "Cargo.lock") {
        push(lockfile);
    }
    inputs
}

/// The `name` file in `start` or the nearest directory above it, or `None`
/// when no ancestor holds one.
fn nearest_ancestor(start: &Path, name: &str) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(name))
        .find(|c| c.is_file())
}

/// Read the dep-info file at `dep_info` and emit one
/// `cargo:rerun-if-changed` line per input the inner build depended on:
/// every source the compiler recorded, each one's owning `Cargo.toml`, and
/// the workspace lockfile above `build_cwd`. Relative prerequisites resolve
/// against `build_cwd`, and the inner build's own outputs under `target_dir`
/// are skipped.
///
/// Call this from a build script immediately after the inner build
/// succeeds, pointing at the `<target-dir>/<triple>/<profile>/<package>.d`
/// rustc wrote beside the linked artefact.
///
/// # Panics
///
/// Panics if the dep-info file cannot be read: the inner build just
/// produced it, so an unreadable file means the caller named the wrong
/// path, and registering *no* inputs would silently freeze the fixture —
/// fail loud instead.
pub fn emit_dep_info_reruns(dep_info: &Path, build_cwd: &Path, target_dir: &Path) {
    let contents = fs::read_to_string(dep_info)
        .unwrap_or_else(|e| panic!("read dep-info {}: {e}", dep_info.display()));
    for input in rerun_inputs(&contents, build_cwd, target_dir) {
        println!("cargo:rerun-if-changed={}", input.display());
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

    /// A workspace shaped like this crate's own: two packages, the second
    /// depending on the first, plus the lockfile that pinned them.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root =
                std::env::temp_dir().join(format!("tairix-depinfo-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for package in ["guest", "shared"] {
                fs::create_dir_all(root.join(package).join("src")).expect("create the package");
                fs::write(root.join(package).join("Cargo.toml"), b"[package]")
                    .expect("write the manifest");
                fs::write(root.join(package).join("src/lib.rs"), b"//!").expect("write the source");
            }
            fs::write(root.join("Cargo.lock"), b"[[package]]").expect("write the lockfile");
            fs::create_dir_all(root.join("guest/target/triple/debug"))
                .expect("create the private target dir");
            Fixture { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// The registered set covers the whole closure the compiler recorded —
    /// the transitive dependency's source included — plus each source's
    /// manifest and the lockfile, and never the inner build's own outputs.
    #[test]
    fn the_input_set_covers_the_closure_its_manifests_and_the_lockfile() {
        let fixture = Fixture::new("closure");
        let root = &fixture.root;
        let target_dir = root.join("guest/target");
        let artefact = target_dir.join("triple/debug/guest");
        let contents = format!(
            "{}: {} {}\n",
            artefact.display(),
            root.join("guest/src/lib.rs").display(),
            root.join("shared/src/lib.rs").display(),
        );

        let inputs = rerun_inputs(&contents, &root.join("guest"), &target_dir);

        assert!(
            inputs.contains(&root.join("shared/src/lib.rs")),
            "a transitive dependency's source must be registered: {inputs:?}"
        );
        assert!(
            inputs.contains(&root.join("shared/Cargo.toml")),
            "its manifest must be registered too: {inputs:?}"
        );
        assert!(
            inputs.contains(&root.join("guest/src/lib.rs")),
            "{inputs:?}"
        );
        assert!(
            inputs.contains(&root.join("guest/Cargo.toml")),
            "{inputs:?}"
        );
        assert!(inputs.contains(&root.join("Cargo.lock")), "{inputs:?}");
        assert!(
            !inputs.iter().any(|p| p.starts_with(&target_dir)),
            "the inner build's own outputs must not be registered: {inputs:?}"
        );
    }

    /// A relative prerequisite resolves against the directory the inner
    /// build ran in, not the outer build script's own.
    #[test]
    fn a_relative_prerequisite_resolves_against_the_inner_build_cwd() {
        let fixture = Fixture::new("relative");
        let root = &fixture.root;
        let inputs = rerun_inputs(
            "target/triple/debug/guest: src/lib.rs\n",
            &root.join("guest"),
            &root.join("guest/target"),
        );
        assert!(
            inputs.contains(&root.join("guest/src/lib.rs")),
            "{inputs:?}"
        );
    }
}
