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
//! runs its `Run` binary. This module computes only the candidate *spelling
//! list* — it performs no I/O and checks no permission, so the policy is
//! exhaustively testable. The [`ProcessHost`](crate::ProcessHost) attempts
//! the candidates in order and owns the trusted load pipeline; the kernel
//! authorises every launch (a candidate list grants nothing).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{BundleEntry, BUNDLE_SUFFIX, SYSTEM_APP_STORE};

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
    let bundle = if word.ends_with(BUNDLE_SUFFIX) {
        String::from(word)
    } else {
        format!("{word}{BUNDLE_SUFFIX}")
    };
    let mut candidates = alloc::vec![format!("{SYSTEM_APP_STORE}/{bundle}/{run}")];
    if let Some(path) = path_var {
        for dir in split_path_entries(path) {
            if dir.is_empty() {
                // POSIX reads an empty entry as the current directory;
                // searching the working directory for commands is a
                // well-known trap, so the entry is skipped, never widened.
                continue;
            }
            let dir = dir.strip_suffix('/').unwrap_or(dir);
            candidates.push(format!("{dir}/{bundle}/{run}"));
        }
    }
    candidates
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
    use super::{resolution_candidates, split_path_entries};
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
}
