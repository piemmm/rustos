//! Command-word resolution: the ordered candidate program paths a typed
//! command word may name (`plans/APPS.md` §8–§9).
//!
//! Every runnable program is an application bundle, `<name>.app`, whose
//! entry point is its `Run` binary. A bare command word therefore resolves
//! to bundle `Run` paths, searched in a fixed, deterministic order:
//!
//! 1. **The system app store** ([`rustos_abi::SYSTEM_APP_STORE`]) — the
//!    OS-provided, read-only, system-signed command apps. Searching it first
//!    is a security property: a user's `PATH` can never shadow a system
//!    command with an attacker-supplied bundle of the same name.
//! 2. **The user's `PATH`** — its directories left to right, each holding
//!    `<word>.app` bundles.
//!
//! A word that spells a *path* (it contains `/`) is explicit and is never
//! searched; a word with a trailing `.app` names the bundle directly and
//! runs its `Run` binary. This crate computes only the candidate *spelling
//! lists* — it performs no I/O and checks no permission, so the policy is
//! exhaustively testable and linking it grants nothing. The shell's process
//! host attempts [`resolution_candidates`] in order and owns the trusted
//! load pipeline; the kernel authorises every launch. The `man` command
//! walks the same order over [`bundle_candidates`] to find the bundle whose
//! `Help/` tree documents a command — one policy, two views, so the page
//! `man` shows always belongs to the program the shell would run. When that
//! ordered list finds nothing, `man` falls back to a recursive walk of the
//! app-store roots [`search_roots`] spells — the machine-wide `/Apps` store,
//! then the user's own `<home>/Apps` — so an installed bundle's help is
//! found however deeply it was filed (`plans/APPS.md` §7).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{BundleEntry, BUNDLE_SUFFIX, SYSTEM_APP_STORE, USER_APP_STORE};

/// Compute the candidate program paths for one command word, in the order
/// they are to be attempted. `path_var` is the value of the `PATH`
/// environment variable, if set.
///
/// The list is deterministic and fail-closed: an empty word yields no
/// candidates, an explicit path yields exactly one, and a bare word yields
/// the system-store spelling followed by one spelling per `PATH` entry.
/// The first candidate the host finds runs; exhausting the list is
/// "command not found".
#[must_use]
pub fn resolution_candidates(word: &str, path_var: Option<&str>) -> Vec<String> {
    if word.is_empty() {
        return Vec::new();
    }
    let run = BundleEntry::Run.as_str();
    if word.contains('/') {
        // An explicit path bypasses the search, exactly as in a POSIX
        // shell. A trailing `.app` names the bundle; run its entry point.
        let candidate = if word.ends_with(BUNDLE_SUFFIX) {
            format!("{word}/{run}")
        } else {
            String::from(word)
        };
        return alloc::vec![candidate];
    }
    bundle_roots(word, path_var)
        .into_iter()
        .map(|root| format!("{root}/{run}"))
        .collect()
}

/// Compute the candidate bundle directories for one command word, in the
/// same order [`resolution_candidates`] attempts their `Run` binaries.
///
/// This is the `man` view of the one policy: `man <word>` reads the `Help/`
/// tree of the first candidate bundle that exists, so the page shown always
/// documents the program the shell would launch for the same word. An empty
/// word yields no candidates; an explicit `.app` path names its bundle
/// directly; an explicit path to a bare program names no bundle at all (it
/// has no `Help/` tree to read), so it yields the empty list rather than a
/// guessed sibling directory.
#[must_use]
pub fn bundle_candidates(word: &str, path_var: Option<&str>) -> Vec<String> {
    if word.is_empty() {
        return Vec::new();
    }
    if word.contains('/') {
        if word.ends_with(BUNDLE_SUFFIX) {
            return alloc::vec![String::from(word)];
        }
        return Vec::new();
    }
    bundle_roots(word, path_var)
}

/// Compute the app-store roots `man`'s recursive bundle search walks, in
/// order, when the [`bundle_candidates`] list finds nothing: the
/// machine-wide user app store (`/Apps`), then the calling user's own
/// `<home>/Apps`. `home` is the inherited `HOME` value, if set.
///
/// Spelling only — the walk itself (and every permission check) belongs to
/// the caller and the kernel. An unset or empty `home` simply contributes
/// no per-user root; nothing is guessed in its place.
#[must_use]
pub fn search_roots(home: Option<&str>) -> Vec<String> {
    let mut roots = alloc::vec![String::from(USER_APP_STORE)];
    if let Some(home) = home {
        let home = home.strip_suffix('/').unwrap_or(home);
        if !home.is_empty() {
            roots.push(format!("{home}/Apps"));
        }
    }
    roots
}

/// The ordered directories a bare (searchable) command word is resolved
/// against: the system app store first, then one per non-empty `PATH` entry
/// (alias-aware split, trailing `/` normalised away). `path_var` is the value
/// of the `PATH` environment variable, if set.
///
/// This is the *directory* view of the one search policy
/// ([`resolution_candidates`] / [`bundle_candidates`] append the
/// `<word>.app` spelling per directory): a completion engine enumerates
/// these directories' bundles so the names it offers are exactly the names
/// the shell would resolve. Spelling only — no I/O, no permission check.
#[must_use]
pub fn command_search_dirs(path_var: Option<&str>) -> Vec<String> {
    let mut dirs = alloc::vec![String::from(SYSTEM_APP_STORE)];
    if let Some(path) = path_var {
        for dir in split_path_entries(path) {
            if dir.is_empty() {
                // POSIX reads an empty entry as the current directory;
                // searching the working directory for commands is a
                // well-known trap, so the entry is skipped, never widened.
                continue;
            }
            let dir = dir.strip_suffix('/').unwrap_or(dir);
            dirs.push(String::from(dir));
        }
    }
    dirs
}

/// The ordered bundle-directory spellings a bare (searchable) word names:
/// the `<word>.app` bundle in each [`command_search_dirs`] directory.
fn bundle_roots(word: &str, path_var: Option<&str>) -> Vec<String> {
    let bundle = if word.ends_with(BUNDLE_SUFFIX) {
        String::from(word)
    } else {
        format!("{word}{BUNDLE_SUFFIX}")
    };
    command_search_dirs(path_var)
        .into_iter()
        .map(|dir| format!("{dir}/{bundle}"))
        .collect()
}

/// Split a `PATH` value into its entries.
///
/// Entries are `:`-separated, but a RustOS alias path itself contains a `:`
/// (`Home:/tools`, `plans/DRIVES.md`), so the split must tell the two
/// apart. The rule is structural and deterministic: a `:` immediately
/// followed by `/` whose preceding text (since the previous separator) is a
/// non-empty name containing no `/` is the alias-name delimiter of that
/// entry, not a separator; every other `:` separates entries. An alias root
/// alone is therefore written `Home:/`, never a bare `Home:`.
fn split_path_entries(path: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let bytes = path.as_bytes();
    let mut start = 0;
    for (i, byte) in bytes.iter().enumerate() {
        if *byte != b':' {
            continue;
        }
        let segment = &path[start..i];
        let alias_delimiter =
            bytes.get(i + 1) == Some(&b'/') && !segment.is_empty() && !segment.contains('/');
        if alias_delimiter {
            continue;
        }
        entries.push(segment);
        start = i + 1;
    }
    entries.push(&path[start..]);
    entries
}

#[cfg(test)]
mod tests {
    use super::{
        bundle_candidates, command_search_dirs, resolution_candidates, search_roots,
        split_path_entries,
    };
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn bare_word_searches_the_store_then_path() {
        let candidates = resolution_candidates("ps", Some("/Users/root/tools:Home:/bin"));
        assert_eq!(
            candidates,
            [
                "/System/Apps/ps.app/Run",
                "/Users/root/tools/ps.app/Run",
                "Home:/bin/ps.app/Run",
            ]
        );
    }

    #[test]
    fn bare_word_without_path_searches_only_the_store() {
        assert_eq!(
            resolution_candidates("top", None),
            ["/System/Apps/top.app/Run"]
        );
    }

    #[test]
    fn app_suffixed_word_names_the_bundle_directly() {
        assert_eq!(
            resolution_candidates("top.app", Some("/opt")),
            ["/System/Apps/top.app/Run", "/opt/top.app/Run"]
        );
    }

    #[test]
    fn explicit_path_bypasses_the_search() {
        assert_eq!(
            resolution_candidates("/System/Apps/ps.app/Run", Some("/opt")),
            ["/System/Apps/ps.app/Run"]
        );
        assert_eq!(resolution_candidates("./tool", Some("/opt")), ["./tool"]);
    }

    #[test]
    fn explicit_bundle_path_runs_its_entry_point() {
        assert_eq!(
            resolution_candidates("/Apps/Example.app", None),
            ["/Apps/Example.app/Run"]
        );
        assert_eq!(
            resolution_candidates("Apps:/Example.app", None),
            ["Apps:/Example.app/Run"]
        );
    }

    #[test]
    fn empty_word_yields_no_candidates() {
        assert_eq!(resolution_candidates("", Some("/opt")), Vec::<&str>::new());
    }

    #[test]
    fn empty_and_trailing_slash_path_entries_are_handled() {
        // Empty entries (the leading, doubled, and trailing `:`) are skipped
        // — never a silent current-directory search — and a trailing `/` on
        // an entry does not double the separator in the spelling.
        assert_eq!(
            resolution_candidates("ps", Some(":/a/::/b")),
            ["/System/Apps/ps.app/Run", "/a/ps.app/Run", "/b/ps.app/Run",]
        );
    }

    #[test]
    fn path_split_keeps_alias_entries_whole() {
        assert_eq!(
            split_path_entries("Home:/tools:/a:/b:Work:/bin"),
            ["Home:/tools", "/a", "/b", "Work:/bin"]
        );
    }

    #[test]
    fn path_split_separates_plain_entries() {
        assert_eq!(split_path_entries("/a:/b"), ["/a", "/b"]);
        assert_eq!(split_path_entries(""), [""]);
        assert_eq!(split_path_entries("a:b"), ["a", "b"]);
        // A bare trailing alias name with no `/` is a separator boundary:
        // an alias root entry is written `Home:/`.
        assert_eq!(split_path_entries("Home:"), ["Home", ""]);
        assert_eq!(vec!["Home:/"], split_path_entries("Home:/"));
    }

    #[test]
    fn bundle_candidates_mirror_the_resolution_order() {
        assert_eq!(
            bundle_candidates("ps", Some("/Users/root/tools:Home:/bin")),
            [
                "/System/Apps/ps.app",
                "/Users/root/tools/ps.app",
                "Home:/bin/ps.app",
            ]
        );
        assert_eq!(bundle_candidates("top", None), ["/System/Apps/top.app"]);
        assert_eq!(
            bundle_candidates("top.app", Some("/opt")),
            ["/System/Apps/top.app", "/opt/top.app"]
        );
    }

    #[test]
    fn explicit_bundle_path_names_its_bundle() {
        assert_eq!(
            bundle_candidates("/Apps/Example.app", Some("/opt")),
            ["/Apps/Example.app"]
        );
        assert_eq!(
            bundle_candidates("Apps:/Example.app", None),
            ["Apps:/Example.app"]
        );
    }

    #[test]
    fn explicit_bare_program_path_names_no_bundle() {
        // A raw program path has no bundle directory and therefore no
        // `Help/` tree; guessing a sibling directory would show help for a
        // program the word does not name.
        assert_eq!(
            bundle_candidates("./tool", Some("/opt")),
            Vec::<&str>::new()
        );
        assert_eq!(
            bundle_candidates("/System/Apps/ps.app/Run", None),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn empty_word_yields_no_bundles() {
        assert_eq!(bundle_candidates("", Some("/opt")), Vec::<&str>::new());
    }

    #[test]
    fn search_roots_are_the_shared_store_then_the_users_own() {
        assert_eq!(
            search_roots(Some("/Users/ada")),
            ["/Apps", "/Users/ada/Apps"]
        );
        // A trailing slash on HOME must not double the separator.
        assert_eq!(
            search_roots(Some("/Users/ada/")),
            ["/Apps", "/Users/ada/Apps"]
        );
    }

    #[test]
    fn a_missing_or_empty_home_contributes_no_per_user_root() {
        assert_eq!(search_roots(None), ["/Apps"]);
        assert_eq!(search_roots(Some("")), ["/Apps"]);
        assert_eq!(search_roots(Some("/")), ["/Apps"]);
    }

    /// The directory view agrees with the candidate view: each search
    /// directory plus the `<word>.app/Run` spelling is exactly the
    /// resolution-candidate list, so completion enumerates precisely the
    /// names the shell would resolve.
    #[test]
    fn search_dirs_agree_with_resolution_candidates() {
        let path = Some("/Users/ada/bin:Home:/tools/");
        let dirs = command_search_dirs(path);
        assert_eq!(dirs, ["/System/Apps", "/Users/ada/bin", "Home:/tools"]);
        let expected: alloc::vec::Vec<_> = dirs
            .iter()
            .map(|dir| alloc::format!("{dir}/ls.app/Run"))
            .collect();
        assert_eq!(resolution_candidates("ls", path), expected);
    }

    /// An empty `PATH` entry (the POSIX "current directory" trap) is skipped
    /// by the directory view exactly as by the candidate view.
    #[test]
    fn search_dirs_skip_empty_path_entries() {
        assert_eq!(
            command_search_dirs(Some(":/Users/ada/bin:")),
            ["/System/Apps", "/Users/ada/bin"]
        );
    }
}
