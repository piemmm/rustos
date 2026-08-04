//! Command-word resolution: the ordered candidate program paths a typed
//! command word may name (`plans/APPS.md` §8–§9).
//!
//! Every runnable program is an application bundle, `<name>.app`, whose
//! entry point is its `Run` binary. A bare command word therefore resolves
//! to bundle `Run` paths, searched in a fixed, deterministic order:
//!
//! 1. **The system command store** ([`tairix_abi::SYSTEM_COMMAND_STORE`]) —
//!    the OS-provided, read-only, system-signed command apps.
//! 2. **The system application store**
//!    ([`tairix_abi::SYSTEM_APPLICATION_STORE`]) — the OS-provided graphical
//!    applications, so a desktop application is typeable by name.
//! 3. **The user's own command store**, `<home>/Commands`.
//! 4. **The user's own application store**, `<home>/Applications`.
//! 5. **The user's `PATH`** — its directories left to right, each holding
//!    `<word>.app` bundles.
//!
//! Steps 1–4 are the *fixed prefix*: their order is a security property, not
//! a convenience. It is built here, not read from the environment, so no
//! `PATH` value, exported variable, or per-user directory can reorder it or
//! shadow a system command with an attacker-supplied bundle of the same
//! name. The two system stores are read-only and system-signed and always
//! precede every user-writable directory; the user's own two stores precede
//! whatever the user has configured. A `PATH` entry that repeats a directory
//! already on the prefix is dropped rather than searched twice, so late
//! `PATH` text cannot move a store's position either.
//!
//! A word that spells a *path* (it contains `/`) is explicit and is never
//! searched; a word with a trailing `.app` names the bundle directly and
//! runs its `Run` binary. This crate computes only the candidate *spelling
//! lists* — it performs no I/O and checks no permission, so the policy is
//! exhaustively testable and linking it grants nothing. A store directory
//! that does not exist (a user who never created `<home>/Commands`) simply
//! contributes candidates nothing is found under; existence is the host's
//! I/O question, never a spelling one. The shell's process host attempts
//! [`resolution_candidates`] in order and owns the trusted load pipeline;
//! the kernel authorises every launch. The `man` command walks the same
//! order over [`bundle_candidates`] to find the bundle whose `Help/` tree
//! documents a command — one policy, two views, so the page `man` shows
//! always belongs to the program the shell would run. When that ordered
//! list finds nothing, `man` falls back to a recursive walk of the roots
//! [`search_roots`] spells — the machine-wide installed store, then the
//! user's own two stores — so a bundle filed in a nested subdirectory is
//! still found (`plans/APPS.md` §7).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{
    BundleEntry, BUNDLE_SUFFIX, HOME_APPLICATION_STORE_DIR, HOME_COMMAND_STORE_DIR,
    INSTALLED_APP_STORE, SYSTEM_APPLICATION_STORE, SYSTEM_COMMAND_STORE,
};

/// The inherited session values the search order reads: the calling user's
/// home root (`HOME`) and their search path (`PATH`), each absent when the
/// variable is unset.
///
/// The two are a named pair rather than two positional arguments of the same
/// type so a call site cannot silently transpose them and search the wrong
/// directories.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommandEnv<'a> {
    /// The user's home root, if `HOME` is set. Their own command and
    /// application stores are its children.
    pub home: Option<&'a str>,
    /// The value of `PATH`, if set: the user-configured directories searched
    /// after the fixed prefix.
    pub path_var: Option<&'a str>,
}

/// Compute the candidate program paths for one command word, in the order
/// they are to be attempted.
///
/// The list is deterministic and fail-closed: an empty word yields no
/// candidates, an explicit path yields exactly one, and a bare word yields
/// one spelling per [`command_search_dirs`] directory. The first candidate
/// the host finds runs; exhausting the list is "command not found".
#[must_use]
pub fn resolution_candidates(word: &str, env: CommandEnv<'_>) -> Vec<String> {
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
    bundle_roots(word, env)
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
pub fn bundle_candidates(word: &str, env: CommandEnv<'_>) -> Vec<String> {
    if word.is_empty() {
        return Vec::new();
    }
    if word.contains('/') {
        if word.ends_with(BUNDLE_SUFFIX) {
            return alloc::vec![String::from(word)];
        }
        return Vec::new();
    }
    bundle_roots(word, env)
}

/// Compute the roots `man`'s recursive bundle search walks, in order, when
/// the [`bundle_candidates`] list finds nothing: the machine-wide installed
/// application store, then the calling user's own command and application
/// stores. `home` is the inherited `HOME` value, if set.
///
/// The two system stores are absent by design: they are flat — one
/// command-named bundle per program — so the ordered candidates already
/// cover them, and there is nothing nested to walk. Spelling only — the walk
/// itself (and every permission check) belongs to the caller and the kernel.
/// An unset or empty `home` simply contributes no per-user root; nothing is
/// guessed in its place.
#[must_use]
pub fn search_roots(home: Option<&str>) -> Vec<String> {
    let mut roots = alloc::vec![String::from(INSTALLED_APP_STORE)];
    roots.extend(home_stores(home));
    roots
}

/// The ordered directories a bare (searchable) command word is resolved
/// against: the fixed prefix — both system stores, then the user's own two
/// stores — followed by one directory per non-empty `PATH` entry
/// (alias-aware split, trailing `/` normalised away, a repeat of a directory
/// already listed dropped).
///
/// This is the *directory* view of the one search policy
/// ([`resolution_candidates`] / [`bundle_candidates`] append the
/// `<word>.app` spelling per directory): a completion engine enumerates
/// these directories' bundles so the names it offers are exactly the names
/// the shell would resolve. Spelling only — no I/O, no permission check.
#[must_use]
pub fn command_search_dirs(env: CommandEnv<'_>) -> Vec<String> {
    let mut dirs = alloc::vec![
        String::from(SYSTEM_COMMAND_STORE),
        String::from(SYSTEM_APPLICATION_STORE),
    ];
    dirs.extend(home_stores(env.home));
    if let Some(path) = env.path_var {
        for dir in split_path_entries(path) {
            if dir.is_empty() {
                // POSIX reads an empty entry as the current directory;
                // searching the working directory for commands is a
                // well-known trap, so the entry is skipped, never widened.
                continue;
            }
            let dir = dir.strip_suffix('/').unwrap_or(dir);
            // A repeat costs a needless lookup per resolution and could
            // otherwise read as moving a store later in the order; the
            // first, authoritative position stands.
            if dirs.iter().any(|listed| listed == dir) {
                continue;
            }
            dirs.push(String::from(dir));
        }
    }
    dirs
}

/// The calling user's own two program stores, in search order, spelled
/// against their home root. An unset, empty, or root-only `home`
/// contributes none: a store is a child of a real home directory, never a
/// guess.
fn home_stores(home: Option<&str>) -> Vec<String> {
    let Some(home) = home.map(|home| home.strip_suffix('/').unwrap_or(home)) else {
        return Vec::new();
    };
    if home.is_empty() {
        return Vec::new();
    }
    [HOME_COMMAND_STORE_DIR, HOME_APPLICATION_STORE_DIR]
        .into_iter()
        .map(|store| format!("{home}/{store}"))
        .collect()
}

/// The ordered bundle-directory spellings a bare (searchable) word names:
/// the `<word>.app` bundle in each [`command_search_dirs`] directory.
fn bundle_roots(word: &str, env: CommandEnv<'_>) -> Vec<String> {
    let bundle = if word.ends_with(BUNDLE_SUFFIX) {
        String::from(word)
    } else {
        format!("{word}{BUNDLE_SUFFIX}")
    };
    command_search_dirs(env)
        .into_iter()
        .map(|dir| format!("{dir}/{bundle}"))
        .collect()
}

/// Split a `PATH` value into its entries.
///
/// Entries are `:`-separated, but a TAIRiX alias path itself contains a `:`
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
        split_path_entries, CommandEnv,
    };
    use alloc::vec;
    use alloc::vec::Vec;

    /// The fixed prefix every bare-word search starts with, for a session
    /// whose home is `/Users/ada`.
    const ADA_PREFIX: [&str; 4] = [
        "/System/Commands",
        "/System/Applications",
        "/Users/ada/Commands",
        "/Users/ada/Applications",
    ];

    fn ada(path_var: Option<&str>) -> CommandEnv<'_> {
        CommandEnv {
            home: Some("/Users/ada"),
            path_var,
        }
    }

    #[test]
    fn bare_word_searches_the_fixed_prefix_then_path() {
        assert_eq!(
            resolution_candidates("ps", ada(Some("/Users/ada/tools:Home:/bin"))),
            [
                "/System/Commands/ps.app/Run",
                "/System/Applications/ps.app/Run",
                "/Users/ada/Commands/ps.app/Run",
                "/Users/ada/Applications/ps.app/Run",
                "/Users/ada/tools/ps.app/Run",
                "Home:/bin/ps.app/Run",
            ]
        );
    }

    #[test]
    fn bare_word_without_path_searches_only_the_fixed_prefix() {
        assert_eq!(
            resolution_candidates("top", ada(None)),
            [
                "/System/Commands/top.app/Run",
                "/System/Applications/top.app/Run",
                "/Users/ada/Commands/top.app/Run",
                "/Users/ada/Applications/top.app/Run",
            ]
        );
    }

    /// With neither `HOME` nor `PATH` the two system stores are still
    /// searched: the prefix is built from the store definitions, never read
    /// from the environment, so an empty environment cannot disarm it.
    #[test]
    fn the_system_stores_are_searched_with_no_environment_at_all() {
        assert_eq!(
            resolution_candidates("ls", CommandEnv::default()),
            [
                "/System/Commands/ls.app/Run",
                "/System/Applications/ls.app/Run",
            ]
        );
    }

    #[test]
    fn app_suffixed_word_names_the_bundle_directly() {
        assert_eq!(
            resolution_candidates("top.app", ada(Some("/opt"))),
            [
                "/System/Commands/top.app/Run",
                "/System/Applications/top.app/Run",
                "/Users/ada/Commands/top.app/Run",
                "/Users/ada/Applications/top.app/Run",
                "/opt/top.app/Run",
            ]
        );
    }

    #[test]
    fn explicit_path_bypasses_the_search() {
        assert_eq!(
            resolution_candidates("/System/Commands/ps.app/Run", ada(Some("/opt"))),
            ["/System/Commands/ps.app/Run"]
        );
        assert_eq!(
            resolution_candidates("./tool", ada(Some("/opt"))),
            ["./tool"]
        );
    }

    #[test]
    fn explicit_bundle_path_runs_its_entry_point() {
        assert_eq!(
            resolution_candidates("/Apps/Example.app", CommandEnv::default()),
            ["/Apps/Example.app/Run"]
        );
        assert_eq!(
            resolution_candidates("Apps:/Example.app", CommandEnv::default()),
            ["Apps:/Example.app/Run"]
        );
    }

    #[test]
    fn empty_word_yields_no_candidates() {
        assert_eq!(
            resolution_candidates("", ada(Some("/opt"))),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn empty_and_trailing_slash_path_entries_are_handled() {
        // Empty entries (the leading, doubled, and trailing `:`) are skipped
        // — never a silent current-directory search — and a trailing `/` on
        // an entry does not double the separator in the spelling.
        assert_eq!(
            command_search_dirs(ada(Some(":/a/::/b"))),
            [ADA_PREFIX.as_slice(), ["/a", "/b"].as_slice()].concat()
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
            bundle_candidates("ps", ada(Some("Home:/bin"))),
            [
                "/System/Commands/ps.app",
                "/System/Applications/ps.app",
                "/Users/ada/Commands/ps.app",
                "/Users/ada/Applications/ps.app",
                "Home:/bin/ps.app",
            ]
        );
        assert_eq!(
            bundle_candidates("top", CommandEnv::default()),
            ["/System/Commands/top.app", "/System/Applications/top.app"]
        );
        assert_eq!(
            bundle_candidates("top.app", CommandEnv::default()),
            ["/System/Commands/top.app", "/System/Applications/top.app"]
        );
    }

    #[test]
    fn explicit_bundle_path_names_its_bundle() {
        assert_eq!(
            bundle_candidates("/Apps/Example.app", ada(Some("/opt"))),
            ["/Apps/Example.app"]
        );
        assert_eq!(
            bundle_candidates("Apps:/Example.app", CommandEnv::default()),
            ["Apps:/Example.app"]
        );
    }

    #[test]
    fn explicit_bare_program_path_names_no_bundle() {
        // A raw program path has no bundle directory and therefore no
        // `Help/` tree; guessing a sibling directory would show help for a
        // program the word does not name.
        assert_eq!(
            bundle_candidates("./tool", ada(Some("/opt"))),
            Vec::<&str>::new()
        );
        assert_eq!(
            bundle_candidates("/System/Commands/ps.app/Run", CommandEnv::default()),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn empty_word_yields_no_bundles() {
        assert_eq!(bundle_candidates("", ada(Some("/opt"))), Vec::<&str>::new());
    }

    #[test]
    fn search_roots_are_the_installed_store_then_the_users_own() {
        assert_eq!(
            search_roots(Some("/Users/ada")),
            ["/Apps", "/Users/ada/Commands", "/Users/ada/Applications"]
        );
        // A trailing slash on HOME must not double the separator.
        assert_eq!(
            search_roots(Some("/Users/ada/")),
            ["/Apps", "/Users/ada/Commands", "/Users/ada/Applications"]
        );
    }

    #[test]
    fn a_missing_or_empty_home_contributes_no_per_user_root() {
        assert_eq!(search_roots(None), ["/Apps"]);
        assert_eq!(search_roots(Some("")), ["/Apps"]);
        assert_eq!(search_roots(Some("/")), ["/Apps"]);
        for home in [None, Some(""), Some("/")] {
            assert_eq!(
                command_search_dirs(CommandEnv {
                    home,
                    path_var: None
                }),
                ["/System/Commands", "/System/Applications"]
            );
        }
    }

    /// The directory view agrees with the candidate view: each search
    /// directory plus the `<word>.app/Run` spelling is exactly the
    /// resolution-candidate list, so completion enumerates precisely the
    /// names the shell would resolve.
    #[test]
    fn search_dirs_agree_with_resolution_candidates() {
        let env = ada(Some("/Users/ada/bin:Home:/tools/"));
        let dirs = command_search_dirs(env);
        assert_eq!(
            dirs,
            [
                ADA_PREFIX.as_slice(),
                ["/Users/ada/bin", "Home:/tools"].as_slice()
            ]
            .concat()
        );
        let expected: alloc::vec::Vec<_> = dirs
            .iter()
            .map(|dir| alloc::format!("{dir}/ls.app/Run"))
            .collect();
        assert_eq!(resolution_candidates("ls", env), expected);
    }

    /// The fixed prefix cannot be reordered, removed, or shadowed by any
    /// `PATH` the user exports: whatever they set, the system command store
    /// is searched first and each prefix directory keeps its position.
    #[test]
    fn no_path_value_can_displace_the_fixed_prefix() {
        for path in [
            "",
            "/attacker",
            "/System/Commands",
            "/Users/ada/Applications:/Users/ada/Commands",
            ":/System/Applications:/attacker:/System/Commands:",
            "/System/Commands/:/Users/ada/Commands/",
        ] {
            let dirs = command_search_dirs(ada(Some(path)));
            assert_eq!(&dirs[..4], ADA_PREFIX.as_slice(), "PATH={path:?}");
            // A repeat of a prefix directory is dropped, so the extra
            // entries are only genuinely new directories.
            let extra: Vec<&str> = dirs[4..]
                .iter()
                .map(alloc::string::String::as_str)
                .collect();
            assert!(
                extra.iter().all(|dir| !ADA_PREFIX.contains(dir)),
                "PATH={path:?} re-listed a prefix directory: {extra:?}"
            );
        }
    }

    /// A directory a user lists twice in `PATH` is searched once, at its
    /// first position: a duplicate only costs lookups and could otherwise
    /// read as moving the directory later in the order.
    #[test]
    fn a_repeated_path_entry_is_searched_once() {
        assert_eq!(
            command_search_dirs(CommandEnv {
                home: None,
                path_var: Some("/a:/b:/a/:/b:/c")
            }),
            ["/System/Commands", "/System/Applications", "/a", "/b", "/c"]
        );
    }
}
